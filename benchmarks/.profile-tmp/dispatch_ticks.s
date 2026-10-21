
target/release/otter:	file format mach-o arm64

Disassembly of section __TEXT,__text:

0000000100dea5c0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks>:
100dea5c0:     	stp	x28, x27, [sp, #-0x60]!
100dea5c4:     	stp	x26, x25, [sp, #0x10]
100dea5c8:     	stp	x24, x23, [sp, #0x20]
100dea5cc:     	stp	x22, x21, [sp, #0x30]
100dea5d0:     	stp	x20, x19, [sp, #0x40]
100dea5d4:     	stp	x29, x30, [sp, #0x50]
100dea5d8:     	add	x29, sp, #0x50
100dea5dc:     	sub	sp, sp, #0x580
100dea5e0:     	ldr	xzr, [sp]
100dea5e4:     	add	x8, x1, #0x8, lsl #12   ; =0x8000
100dea5e8:     	ldrb	w9, [x1, #0xc8]
100dea5ec:     	cmp	w9, #0x0
100dea5f0:     	ldr	x10, [x1, #0x7f30]
100dea5f4:     	ldr	x11, [x1, #0x7668]
100dea5f8:     	ldr	x12, [x8, #0x370]
100dea5fc:     	ccmn	x11, #0x1, #0x0, eq
100dea600:     	ccmp	x12, #0x0, #0x0, eq
100dea604:     	cset	w13, ne
100dea608:     	ldr	x8, [x1, #0x77f0]
100dea60c:     	ldapurb	w8, [x8, #0x18]
100dea610:     	cbz	w8, 0x100dea628 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68>
100dea614:     	mov	w8, #0xa                ; =10
100dea618:     	strb	w8, [x0, #0x8]
100dea61c:     	mov	w8, #0x1                ; =1
100dea620:     	str	x8, [x0]
100dea624:     	b	0x100df1dbc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x77fc>
100dea628:     	mov	x25, x3
100dea62c:     	mov	x19, x2
100dea630:     	mov	x20, x1
100dea634:     	str	x10, [sp, #0x340]
100dea638:     	str	x7, [sp, #0x330]
100dea63c:     	str	x5, [sp, #0x328]
100dea640:     	str	w9, [sp, #0x320]
100dea644:     	str	x11, [sp, #0x318]
100dea648:     	str	x12, [sp, #0x310]
100dea64c:     	str	w13, [sp, #0x324]
100dea650:     	cbz	x3, 0x100dea68c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xcc>
100dea654:     	ldrb	w8, [x4, #0xb]
100dea658:     	tbnz	w8, #0x1, 0x100dea68c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xcc>
100dea65c:     	add	x26, x20, #0x8, lsl #12 ; =0x8000
100dea660:     	ldr	w24, [x4, #0x50]
100dea664:     	cbnz	w24, 0x100dea7b8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1f8>
100dea668:     	ldr	x8, [x4, #0x48]
100dea66c:     	cbz	x8, 0x100dea678 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xb8>
100dea670:     	ldr	w8, [x8, #0x50]
100dea674:     	cbz	w8, 0x100dea6cc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x10c>
100dea678:     	ldrb	w9, [x4, #0xa]
100dea67c:     	cmp	w9, #0x3
100dea680:     	cinc	w24, w8, ne
100dea684:     	cbnz	w24, 0x100dea7b4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1f4>
100dea688:     	b	0x100dea7b8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1f8>
100dea68c:     	ldr	x8, [x5, #0x180]
100dea690:     	cbz	x8, 0x100dea6a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xe8>
100dea694:     	ldr	x4, [x8, #0x8]
100dea698:     	cbz	x4, 0x100dea748 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x188>
100dea69c:     	ldrb	w8, [x4, #0xb]
100dea6a0:     	tbz	w8, #0x1, 0x100dea718 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x158>
100dea6a4:     	b	0x100dea710 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x150>
100dea6a8:     	add	x26, x20, #0x8, lsl #12 ; =0x8000
100dea6ac:     	mov	w24, #0x0               ; =0
100dea6b0:     	mov	x4, #0x0                ; =0
100dea6b4:     	mov	x25, #0x0               ; =0
100dea6b8:     	mov	w8, w24
100dea6bc:     	str	x4, [sp, #0x358]
100dea6c0:     	cmp	x6, x8
100dea6c4:     	b.lo	0x100dea7c8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x208>
100dea6c8:     	b	0x100dea764 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1a4>
100dea6cc:     	mov	x23, x6
100dea6d0:     	mov	x21, x0
100dea6d4:     	mov	x0, x4
100dea6d8:     	mov	x22, x19
100dea6dc:     	mov	x19, x4
100dea6e0:     	bl	0x101b64e08 <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm10native_abi5frameNtB5_5Frame20derive_logical_depth>
100dea6e4:     	mov	x4, x19
100dea6e8:     	mov	x19, x22
100dea6ec:     	mov	x24, x0
100dea6f0:     	mov	x0, x21
100dea6f4:     	mov	x6, x23
100dea6f8:     	ldr	x5, [sp, #0x328]
100dea6fc:     	mov	w8, w24
100dea700:     	str	x4, [sp, #0x358]
100dea704:     	cmp	x23, x8
100dea708:     	b.lo	0x100dea7c8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x208>
100dea70c:     	b	0x100dea764 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1a4>
100dea710:     	ldr	x4, [x4, #0x48]
100dea714:     	cbz	x4, 0x100dea748 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x188>
100dea718:     	ldrb	w8, [x4, #0xa]
100dea71c:     	cmp	w8, #0x3
100dea720:     	b.eq	0x100dea710 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x150>
100dea724:     	add	x26, x20, #0x8, lsl #12 ; =0x8000
100dea728:     	ldr	w24, [x4, #0x50]
100dea72c:     	cbz	w24, 0x100dea790 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1d0>
100dea730:     	mov	x25, #0x0               ; =0
100dea734:     	mov	w8, w24
100dea738:     	str	x4, [sp, #0x358]
100dea73c:     	cmp	x6, x8
100dea740:     	b.lo	0x100dea7c8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x208>
100dea744:     	b	0x100dea764 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1a4>
100dea748:     	add	x26, x20, #0x8, lsl #12 ; =0x8000
100dea74c:     	mov	w24, #0x0               ; =0
100dea750:     	mov	x25, #0x0               ; =0
100dea754:     	mov	w8, w24
100dea758:     	str	x4, [sp, #0x358]
100dea75c:     	cmp	x6, x8
100dea760:     	b.lo	0x100dea7c8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x208>
100dea764:     	ldr	w8, [x20, #0x118]
100dea768:     	ldr	x9, [x20, #0x120]
100dea76c:     	str	xzr, [x20, #0x118]
100dea770:     	cmp	w8, #0x0
100dea774:     	mov	w8, #0xa                ; =10
100dea778:     	csel	x8, x9, x8, ne
100dea77c:     	mov	w9, #0x17               ; =23
100dea780:     	strb	w9, [x0, #0x8]
100dea784:     	str	x8, [x0, #0x10]
100dea788:     	str	xzr, [x0]
100dea78c:     	b	0x100df1dbc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x77fc>
100dea790:     	ldr	x8, [x4, #0x48]
100dea794:     	cbz	x8, 0x100dea7ac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1ec>
100dea798:     	ldr	w8, [x8, #0x50]
100dea79c:     	cbz	w8, 0x100df1c50 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7690>
100dea7a0:     	adds	w24, w8, #0x1
100dea7a4:     	b.hs	0x100dea74c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x18c>
100dea7a8:     	b	0x100dea7b0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1f0>
100dea7ac:     	mov	w24, #0x1               ; =1
100dea7b0:     	mov	x25, #0x0               ; =0
100dea7b4:     	str	w24, [x4, #0x50]
100dea7b8:     	mov	w8, w24
100dea7bc:     	str	x4, [sp, #0x358]
100dea7c0:     	cmp	x6, x8
100dea7c4:     	b.hs	0x100dea764 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1a4>
100dea7c8:     	sub	x8, x8, #0x1
100dea7cc:     	str	x8, [sp, #0x360]
100dea7d0:     	ldr	w2, [x4]
100dea7d4:     	str	w2, [sp, #0x36c]
100dea7d8:     	str	x0, [sp, #0x2f8]
100dea7dc:     	cbz	x25, 0x100df0c70 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x66b0>
100dea7e0:     	ldr	w8, [x25, #0x118]
100dea7e4:     	cmp	w8, w2
100dea7e8:     	b.ne	0x100df0d20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6760>
100dea7ec:     	str	wzr, [sp, #0x2f4]
100dea7f0:     	ldr	x8, [x29, #0x10]
100dea7f4:     	str	x8, [sp, #0x2e8]
100dea7f8:     	ldr	w8, [x26, #0x1d8]
100dea7fc:     	cmp	w8, w24
100dea800:     	b.hs	0x100dea808 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x248>
100dea804:     	str	w24, [x26, #0x1d8]
100dea808:     	mov	x9, #0x0                ; =0
100dea80c:     	ldr	x8, [x25, #0xf8]
100dea810:     	str	x8, [sp, #0x2e0]
100dea814:     	ldr	x8, [x5, #0x10]
100dea818:     	cmp	x8, #0x2
100dea81c:     	str	xzr, [sp, #0x350]
100dea820:     	b.ne	0x100df0dd4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6814>
100dea824:     	ldrb	w8, [x5, #0x112]
100dea828:     	str	x9, [sp, #0x338]
100dea82c:     	cmp	w8, #0xff
100dea830:     	b.ne	0x100df0dd8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6818>
100dea834:     	str	xzr, [sp, #0x2a0]
100dea838:     	str	xzr, [sp, #0x298]
100dea83c:     	str	xzr, [sp, #0x290]
100dea840:     	str	xzr, [sp, #0x2a8]
100dea844:     	str	xzr, [sp, #0x278]
100dea848:     	str	xzr, [sp, #0x2c0]
100dea84c:     	str	xzr, [sp, #0x338]
100dea850:     	str	xzr, [sp, #0x350]
100dea854:     	mov	w8, #0x7668             ; =30312
100dea858:     	add	x8, x20, x8
100dea85c:     	str	x8, [sp, #0x300]
100dea860:     	mov	w8, #0x7670             ; =30320
100dea864:     	add	x8, x20, x8
100dea868:     	str	x8, [sp, #0x2d8]
100dea86c:     	str	x8, [sp, #0x2b8]
100dea870:     	str	x8, [sp, #0x2b0]
100dea874:     	str	x8, [sp, #0x2d0]
100dea878:     	str	x8, [sp, #0x288]
100dea87c:     	str	x8, [sp, #0x280]
100dea880:     	str	x19, [sp, #0x308]
100dea884:     	str	x6, [sp, #0x2c8]
100dea888:     	ldr	x8, [sp, #0x358]
100dea88c:     	ldrb	w9, [x8, #0xb]
100dea890:     	tbnz	w9, #0x1, 0x100df0e6c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68ac>
100dea894:     	ldr	w21, [x8, #0x4]
100dea898:     	ldr	x8, [sp, #0x330]
100dea89c:     	str	w21, [x8]
100dea8a0:     	ldr	x8, [x25, #0x78]
100dea8a4:     	cmp	x8, x21
100dea8a8:     	b.ls	0x100df0de8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6828>
100dea8ac:     	ldr	x8, [sp, #0x340]
100dea8b0:     	cbz	x8, 0x100dea8e0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x320>
100dea8b4:     	ldr	x8, [sp, #0x338]
100dea8b8:     	add	x8, x8, #0x1
100dea8bc:     	str	x8, [sp, #0x338]
100dea8c0:     	ldr	x8, [x25, #0xd8]
100dea8c4:     	cmp	x8, x21
100dea8c8:     	b.ls	0x100dea8e0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x320>
100dea8cc:     	add	x8, x25, #0xd0
100dea8d0:     	ldr	x8, [x8]
100dea8d4:     	add	x26, x8, x21, lsl #2
100dea8d8:     	add	x28, x25, #0xd0
100dea8dc:     	b	0x100dea8e4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x324>
100dea8e0:     	mov	x28, #0x0               ; =0
100dea8e4:     	ldr	x8, [x25, #0x70]
100dea8e8:     	add	x27, x8, x21, lsl #5
100dea8ec:     	ldrb	w24, [x27, #0x1c]
100dea8f0:     	ldrb	w8, [x27, #0x1e]
100dea8f4:     	ldr	x9, [sp, #0x350]
100dea8f8:     	add	x9, x9, x8
100dea8fc:     	str	x9, [sp, #0x350]
100dea900:     	ldr	w8, [sp, #0x324]
100dea904:     	tbz	w8, #0x0, 0x100deaa9c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4dc>
100dea908:     	ldr	w8, [sp, #0x320]
100dea90c:     	cbz	w8, 0x100dea954 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x394>
100dea910:     	add	x10, x20, #0x8, lsl #12 ; =0x8000
100dea914:     	ldr	x8, [x10, #0x108]
100dea918:     	ldr	x9, [sp, #0x350]
100dea91c:     	add	x8, x8, x9
100dea920:     	str	x8, [x10, #0x108]
100dea924:     	str	xzr, [sp, #0x350]
100dea928:     	add	x0, sp, #0x420
100dea92c:     	mov	x1, x20
100dea930:     	bl	0x100d96c0c <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp5statsNtB6_11Interpreter27poll_work_budget_checkpoint>
100dea934:     	ldrb	w9, [sp, #0x420]
100dea938:     	ldrb	w8, [sp, #0x421]
100dea93c:     	cmp	w9, #0xff
100dea940:     	b.ne	0x100df0e3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x687c>
100dea944:     	tbz	w8, #0x0, 0x100dea968 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x3a8>
100dea948:     	str	xzr, [sp, #0x350]
100dea94c:     	mov	x0, x20
100dea950:     	bl	0x100d96560 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp5statsNtB6_11Interpreter24rotate_work_budget_slice>
100dea954:     	ldr	x8, [sp, #0x318]
100dea958:     	cmn	x8, #0x1
100dea95c:     	ldr	x19, [sp, #0x308]
100dea960:     	b.ne	0x100dea97c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x3bc>
100dea964:     	b	0x100deaa50 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x490>
100dea968:     	str	xzr, [sp, #0x350]
100dea96c:     	ldr	x8, [sp, #0x318]
100dea970:     	cmn	x8, #0x1
100dea974:     	ldr	x19, [sp, #0x308]
100dea978:     	b.eq	0x100deaa50 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x490>
100dea97c:     	ldr	x9, [sp, #0x300]
100dea980:     	ldr	x8, [x9]
100dea984:     	mov	x10, #-0x1              ; =-1
100dea988:     	str	x10, [x9]
100dea98c:     	cmn	x8, #0x1
100dea990:     	b.eq	0x100deaa50 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x490>
100dea994:     	ldr	x9, [sp, #0x2d8]
100dea998:     	ldp	q0, q1, [x9, #0x40]
100dea99c:     	add	x10, sp, #0x388
100dea9a0:     	stur	q0, [x10, #0x48]
100dea9a4:     	stur	q1, [x10, #0x58]
100dea9a8:     	ldp	q0, q1, [x9]
100dea9ac:     	stur	q0, [x10, #0x8]
100dea9b0:     	stur	q1, [x10, #0x18]
100dea9b4:     	ldp	q0, q1, [x9, #0x20]
100dea9b8:     	stur	q0, [x10, #0x28]
100dea9bc:     	str	x8, [sp, #0x388]
100dea9c0:     	ldr	x8, [x9, #0x60]
100dea9c4:     	stur	x8, [x10, #0x68]
100dea9c8:     	stur	q1, [x10, #0x38]
100dea9cc:     	add	x0, sp, #0x388
100dea9d0:     	mov	x1, x20
100dea9d4:     	mov	x2, x19
100dea9d8:     	bl	0x100e1f488 <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm11cpu_profileNtB4_11CpuProfiler12maybe_sample>
100dea9dc:     	ldr	x8, [sp, #0x300]
100dea9e0:     	ldr	x8, [x8]
100dea9e4:     	cmn	x8, #0x1
100dea9e8:     	b.eq	0x100deaa04 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x444>
100dea9ec:     	ldr	x0, [sp, #0x300]
100dea9f0:     	bl	0x100c47104 <__RINvNtCsjIpoRzUjyHQ_4core3ptr9drop_glueINtNtCs6zY7rJBZy9D_5alloc3vec3VecIBC_NtNtCsgTc3P8JF8aG_8otter_vm11run_control18StackFrameSnapshotEEEB1g_>
100dea9f4:     	ldr	x8, [x20, #0x7680]
100dea9f8:     	cbz	x8, 0x100deaa04 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x444>
100dea9fc:     	ldr	x0, [x20, #0x7688]
100deaa00:     	bl	0x101baa2b4 <_mi_free>
100deaa04:     	add	x8, sp, #0x2c9
100deaa08:     	ldur	q0, [x8, #0xff]
100deaa0c:     	add	x8, sp, #0x2d9
100deaa10:     	ldur	q1, [x8, #0xff]
100deaa14:     	ldr	x8, [sp, #0x300]
100deaa18:     	stp	q0, q1, [x8, #0x40]
100deaa1c:     	add	x9, sp, #0x2e9
100deaa20:     	ldur	q0, [x9, #0xff]
100deaa24:     	str	q0, [x8, #0x60]
100deaa28:     	add	x9, sp, #0x289
100deaa2c:     	ldur	q0, [x9, #0xff]
100deaa30:     	add	x9, sp, #0x299
100deaa34:     	ldur	q1, [x9, #0xff]
100deaa38:     	stp	q0, q1, [x8]
100deaa3c:     	add	x9, sp, #0x2a9
100deaa40:     	ldur	q0, [x9, #0xff]
100deaa44:     	add	x9, sp, #0x2b9
100deaa48:     	ldur	q1, [x9, #0xff]
100deaa4c:     	stp	q0, q1, [x8, #0x20]
100deaa50:     	ldr	x8, [sp, #0x310]
100deaa54:     	cbz	x8, 0x100deaa9c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4dc>
100deaa58:     	ldr	x6, [sp, #0x360]
100deaa5c:     	ldr	w8, [sp, #0x36c]
100deaa60:     	add	x9, x20, #0x8, lsl #12  ; =0x8000
100deaa64:     	ldr	x1, [x9, #0x370]
100deaa68:     	ldr	x2, [x9, #0x378]
100deaa6c:     	ldr	x3, [x19]
100deaa70:     	ldr	w4, [x19, #0x10]
100deaa74:     	stp	x21, x27, [sp, #0x8]
100deaa78:     	add	x0, sp, #0x420
100deaa7c:     	str	w8, [sp]
100deaa80:     	ldr	x5, [sp, #0x328]
100deaa84:     	mov	x7, x25
100deaa88:     	bl	0x100df2020 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter15emit_step_trace>
100deaa8c:     	ldrb	w8, [sp, #0x420]
100deaa90:     	cmp	w8, #0xff
100deaa94:     	b.ne	0x100df0e20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6860>
100deaa98:     	ldr	x19, [sp, #0x308]
100deaa9c:     	str	w22, [sp, #0x348]
100deaaa0:     	adrp	x10, 0x102d0d000 <_anon.49087316163ed4345739c0711faf6653.2247+0x1889>
100deaaa4:     	add	x10, x10, #0x9c2
100deaaa8:     	adr	x8, 0x100deaab8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4f8>
100deaaac:     	ldrh	w9, [x10, x24, lsl #1]
100deaab0:     	add	x8, x8, x9, lsl #2
100deaab4:     	br	x8
100deaab8:     	ldrh	w22, [x27, #0xc]
100deaabc:     	ldrh	w8, [x27, #0x10]
100deaac0:     	ldr	x23, [sp, #0x358]
100deaac4:     	ldr	x9, [x23, #0x10]
100deaac8:     	ldr	x21, [x9, x8, lsl #3]
100deaacc:     	ldrh	w10, [x27, #0x14]
100deaad0:     	ldr	x27, [x9, x10, lsl #3]
100deaad4:     	and	x9, x21, #0xfffe000000000000
100deaad8:     	mov	x11, #-0x2000000000000  ; =-562949953421312
100deaadc:     	cmp	x9, x11
100deaae0:     	b.eq	0x100deabb0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5f0>
100deaae4:     	cbz	x9, 0x100ded258 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c98>
100deaae8:     	mov	x9, #-0x2000000000000   ; =-562949953421312
100deaaec:     	add	x12, x21, x9
100deaaf0:     	mov	w11, #0x1               ; =1
100deaaf4:     	stp	w11, w21, [x29, #-0x78]
100deaaf8:     	stur	x12, [x29, #-0x70]
100deaafc:     	and	x9, x27, #0xfffe000000000000
100deab00:     	mov	x13, #-0x2000000000000  ; =-562949953421312
100deab04:     	cmp	x9, x13
100deab08:     	b.ne	0x100deabcc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x60c>
100deab0c:     	mov	w9, #0x0                ; =0
100deab10:     	fmov	d0, x12
100deab14:     	stp	w9, w27, [x29, #-0xa8]
100deab18:     	fmov	d1, x13
100deab1c:     	stur	x13, [x29, #-0xa0]
100deab20:     	cmp	w24, #0x12
100deab24:     	b.le	0x100deabf4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x634>
100deab28:     	cmp	w24, #0x13
100deab2c:     	b.eq	0x100deb3d8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xe18>
100deab30:     	cmp	w24, #0x14
100deab34:     	b.eq	0x100deb370 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xdb0>
100deab38:     	cmp	w24, #0x16
100deab3c:     	b.ne	0x100ded258 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c98>
100deab40:     	add	x8, sp, #0x420
100deab44:     	sub	x0, x29, #0x78
100deab48:     	sub	x1, x29, #0xa8
100deab4c:     	bl	0x100fbef90 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6number7bitwise3pow>
100deab50:     	ldr	w8, [sp, #0x420]
100deab54:     	b	0x100deb3a0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xde0>
100deab58:     	ldrh	w5, [x27, #0xc]
100deab5c:     	ldrh	w6, [x27, #0x10]
100deab60:     	ldr	x4, [sp, #0x358]
100deab64:     	ldr	x8, [x4, #0x10]
100deab68:     	ldr	x9, [x8, x6, lsl #3]
100deab6c:     	ldrh	w7, [x27, #0x14]
100deab70:     	ldr	x8, [x8, x7, lsl #3]
100deab74:     	and	x10, x9, #0xfffe000000000000
100deab78:     	mov	x11, #-0x2000000000000  ; =-562949953421312
100deab7c:     	cmp	x10, x11
100deab80:     	b.eq	0x100deac2c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x66c>
100deab84:     	ldr	x2, [sp, #0x328]
100deab88:     	ldr	w22, [sp, #0x348]
100deab8c:     	cbz	x10, 0x100ded30c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2d4c>
100deab90:     	mov	x10, #-0x2000000000000  ; =-562949953421312
100deab94:     	add	x10, x9, x10
100deab98:     	fmov	d0, x10
100deab9c:     	and	x10, x8, #0xfffe000000000000
100deaba0:     	cmp	x10, x11
100deaba4:     	b.ne	0x100deac44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x684>
100deaba8:     	scvtf	d1, w8
100deabac:     	b	0x100deac54 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x694>
100deabb0:     	mov	w11, #0x0               ; =0
100deabb4:     	stp	w11, w21, [x29, #-0x78]
100deabb8:     	stur	x12, [x29, #-0x70]
100deabbc:     	and	x9, x27, #0xfffe000000000000
100deabc0:     	mov	x13, #-0x2000000000000  ; =-562949953421312
100deabc4:     	cmp	x9, x13
100deabc8:     	b.eq	0x100deab0c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x54c>
100deabcc:     	cbz	x9, 0x100ded258 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c98>
100deabd0:     	mov	x9, #-0x2000000000000   ; =-562949953421312
100deabd4:     	add	x13, x27, x9
100deabd8:     	mov	w9, #0x1                ; =1
100deabdc:     	fmov	d0, x12
100deabe0:     	stp	w9, w27, [x29, #-0xa8]
100deabe4:     	fmov	d1, x13
100deabe8:     	stur	x13, [x29, #-0xa0]
100deabec:     	cmp	w24, #0x12
100deabf0:     	b.gt	0x100deab28 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x568>
100deabf4:     	cmp	w24, #0x11
100deabf8:     	b.eq	0x100deb3b8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xdf8>
100deabfc:     	cmp	w24, #0x12
100deac00:     	b.ne	0x100ded258 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c98>
100deac04:     	tbnz	w11, #0x0, 0x100deac10 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x650>
100deac08:     	cbz	w9, 0x100dec968 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x23a8>
100deac0c:     	scvtf	d0, w21
100deac10:     	scvtf	d2, w27
100deac14:     	cmp	w9, #0x0
100deac18:     	fcsel	d1, d1, d2, ne
100deac1c:     	fmul	d0, d1, d0
100deac20:     	str	d0, [sp, #0x428]
100deac24:     	mov	w8, #0x1                ; =1
100deac28:     	b	0x100deb3a0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xde0>
100deac2c:     	scvtf	d0, w9
100deac30:     	ldr	x2, [sp, #0x328]
100deac34:     	ldr	w22, [sp, #0x348]
100deac38:     	and	x10, x8, #0xfffe000000000000
100deac3c:     	cmp	x10, x11
100deac40:     	b.eq	0x100deaba8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5e8>
100deac44:     	cbz	x10, 0x100ded30c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2d4c>
100deac48:     	mov	x10, #-0x2000000000000  ; =-562949953421312
100deac4c:     	add	x10, x8, x10
100deac50:     	fmov	d1, x10
100deac54:     	fcmp	d0, d1
100deac58:     	cset	w10, gt
100deac5c:     	cset	w11, ge
100deac60:     	cmp	w24, #0x23
100deac64:     	csel	w10, w10, w11, eq
100deac68:     	fcmp	d0, d1
100deac6c:     	cset	w11, mi
100deac70:     	cset	w12, ls
100deac74:     	cmp	w24, #0x21
100deac78:     	csel	w11, w11, w12, eq
100deac7c:     	cmp	w24, #0x22
100deac80:     	csel	w10, w10, w11, gt
100deac84:     	cmp	w10, #0x0
100deac88:     	mov	w10, #0x6               ; =6
100deac8c:     	cinc	x10, x10, ne
100deac90:     	cbz	x28, 0x100deace4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x724>
100deac94:     	mov	x11, #-0x2000000000001  ; =-562949953421313
100deac98:     	cmp	x9, x11
100deac9c:     	mov	w9, #0x1                ; =1
100deaca0:     	cinc	w12, w9, ls
100deaca4:     	cmp	x8, x11
100deaca8:     	cinc	w8, w9, ls
100deacac:     	orr	w8, w8, w12
100deacb0:     	ldsetb	w8, w9, [x26]
100deacb4:     	bics	wzr, w8, w9
100deacb8:     	b.eq	0x100deace4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x724>
100deacbc:     	ldr	w8, [x28, #0x20]
100deacc0:     	cmn	w8, #0x1
100deacc4:     	b.eq	0x100deace4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x724>
100deacc8:     	add	w9, w8, #0x1
100deaccc:     	add	x11, x28, #0x20
100deacd0:     	mov	x12, x8
100deacd4:     	casl	w12, w9, [x11]
100deacd8:     	cmp	w12, w8
100deacdc:     	mov	x8, x12
100deace0:     	b.ne	0x100deacc0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x700>
100deace4:     	ldr	x8, [sp, #0x358]
100deace8:     	ldr	x9, [x8, #0x10]
100deacec:     	str	x10, [x9, x5, lsl #3]
100deacf0:     	b	0x100ded210 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c50>
100deacf4:     	ldrh	w5, [x27, #0xc]
100deacf8:     	ldrh	w6, [x27, #0x10]
100deacfc:     	ldr	x4, [sp, #0x358]
100dead00:     	ldr	x8, [x4, #0x10]
100dead04:     	ldr	x9, [x8, x6, lsl #3]
100dead08:     	ldrh	w7, [x27, #0x14]
100dead0c:     	ldr	x8, [x8, x7, lsl #3]
100dead10:     	and	x10, x9, #0xfffe000000000000
100dead14:     	mov	x11, #-0x2000000000000  ; =-562949953421312
100dead18:     	cmp	x10, x11
100dead1c:     	b.eq	0x100deb564 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xfa4>
100dead20:     	ldr	x2, [sp, #0x328]
100dead24:     	ldr	w22, [sp, #0x348]
100dead28:     	cbz	x10, 0x100defad0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5510>
100dead2c:     	mov	x10, #-0x2000000000000  ; =-562949953421312
100dead30:     	add	x10, x9, x10
100dead34:     	fmov	d1, x10
100dead38:     	mov	w13, #0x1               ; =1
100dead3c:     	b	0x100deb574 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xfb4>
100dead40:     	ldr	w10, [x27, #0xc]
100dead44:     	ldr	x8, [sp, #0x358]
100dead48:     	ldrh	w11, [x8, #0x8]
100dead4c:     	ldr	x9, [x8, #0x10]
100dead50:     	cmp	w11, w10, uxth
100dead54:     	ccmp	x9, #0x0, #0x4, hi
100dead58:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100dead5c:     	and	x10, x10, #0xffff
100dead60:     	mov	w11, #0x7               ; =7
100dead64:     	b	0x100deb354 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xd94>
100dead68:     	ldrh	w5, [x27, #0xc]
100dead6c:     	ldrh	w6, [x27, #0x10]
100dead70:     	ldr	x4, [sp, #0x358]
100dead74:     	ldr	x8, [x4, #0x10]
100dead78:     	ldr	x9, [x8, x6, lsl #3]
100dead7c:     	ldrh	w7, [x27, #0x14]
100dead80:     	ldr	x8, [x8, x7, lsl #3]
100dead84:     	and	x10, x9, #0xfffe000000000000
100dead88:     	mov	x11, #-0x2000000000000  ; =-562949953421312
100dead8c:     	cmp	x10, x11
100dead90:     	b.eq	0x100deb59c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xfdc>
100dead94:     	ldr	x2, [sp, #0x328]
100dead98:     	ldr	w22, [sp, #0x348]
100dead9c:     	cbz	x10, 0x100defb00 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5540>
100deada0:     	mov	x10, #-0x2000000000000  ; =-562949953421312
100deada4:     	add	x10, x9, x10
100deada8:     	fmov	d1, x10
100deadac:     	mov	w13, #0x1               ; =1
100deadb0:     	b	0x100deb5ac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xfec>
100deadb4:     	ldrh	w5, [x27, #0xc]
100deadb8:     	ldrh	w6, [x27, #0x10]
100deadbc:     	ldr	x4, [sp, #0x358]
100deadc0:     	ldr	x8, [x4, #0x10]
100deadc4:     	ldr	x10, [x8, x6, lsl #3]
100deadc8:     	ldrh	w7, [x27, #0x14]
100deadcc:     	ldr	x9, [x8, x7, lsl #3]
100deadd0:     	and	x8, x10, #0xfffe000000000000
100deadd4:     	mov	x11, #-0x2000000000000  ; =-562949953421312
100deadd8:     	cmp	x8, x11
100deaddc:     	b.eq	0x100deb5d4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1014>
100deade0:     	ldr	x2, [sp, #0x328]
100deade4:     	ldr	w22, [sp, #0x348]
100deade8:     	cbz	x8, 0x100defb18 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5558>
100deadec:     	mov	x8, #-0x2000000000000   ; =-562949953421312
100deadf0:     	add	x8, x10, x8
100deadf4:     	fmov	d0, x8
100deadf8:     	mov	w8, #0x1                ; =1
100deadfc:     	b	0x100deb5e0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1020>
100deae00:     	ldrh	w5, [x27, #0xc]
100deae04:     	ldrh	w6, [x27, #0x10]
100deae08:     	ldr	w7, [x27, #0x14]
100deae0c:     	ldr	x4, [sp, #0x358]
100deae10:     	ldr	x8, [x4, #0x10]
100deae14:     	ldr	x8, [x8, x6, lsl #3]
100deae18:     	and	x9, x8, #0xfffe000000000000
100deae1c:     	mov	x10, #-0x2000000000000  ; =-562949953421312
100deae20:     	cmp	x9, x10
100deae24:     	b.eq	0x100deb610 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1050>
100deae28:     	ldr	w22, [sp, #0x348]
100deae2c:     	cbz	x9, 0x100defe64 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x58a4>
100deae30:     	mov	x9, #-0x2000000000000   ; =-562949953421312
100deae34:     	add	x9, x8, x9
100deae38:     	fmov	d0, x9
100deae3c:     	scvtf	d1, w7
100deae40:     	fsub	d0, d0, d1
100deae44:     	fcvtzs	w9, d0
100deae48:     	scvtf	d1, w9
100deae4c:     	fcmp	d0, d1
100deae50:     	b.ne	0x100ded0f4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2b34>
100deae54:     	cbnz	w9, 0x100deb61c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x105c>
100deae58:     	fmov	x10, d0
100deae5c:     	tbnz	x10, #0x3f, 0x100ded0f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2b38>
100deae60:     	b	0x100deb61c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x105c>
100deae64:     	ldr	w10, [x27, #0xc]
100deae68:     	ldr	x8, [sp, #0x358]
100deae6c:     	ldrh	w11, [x8, #0x8]
100deae70:     	ldr	x9, [x8, #0x10]
100deae74:     	cmp	w11, w10, uxth
100deae78:     	ccmp	x9, #0x0, #0x4, hi
100deae7c:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100deae80:     	and	x10, x10, #0xffff
100deae84:     	mov	w11, #0x2               ; =2
100deae88:     	str	x11, [x9, x10, lsl #3]
100deae8c:     	ldr	w9, [x8, #0x4]
100deae90:     	cmn	w9, #0x1
100deae94:     	b.ne	0x100deb364 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xda4>
100deae98:     	b	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100deae9c:     	ldr	w10, [x27, #0xc]
100deaea0:     	ldr	x8, [sp, #0x358]
100deaea4:     	ldrh	w11, [x8, #0x8]
100deaea8:     	ldr	x9, [x8, #0x10]
100deaeac:     	cmp	w11, w10, uxth
100deaeb0:     	ccmp	x9, #0x0, #0x4, hi
100deaeb4:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100deaeb8:     	and	x10, x10, #0xffff
100deaebc:     	ldr	w11, [x27, #0x10]
100deaec0:     	orr	x11, x11, #0xfffe000000000000
100deaec4:     	b	0x100deb354 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xd94>
100deaec8:     	ldrh	w8, [x27, #0x10]
100deaecc:     	ldr	x9, [sp, #0x358]
100deaed0:     	ldr	x10, [x9, #0x10]
100deaed4:     	ldr	x8, [x10, x8, lsl #3]
100deaed8:     	ldrh	w11, [x27, #0xc]
100deaedc:     	b	0x100deb100 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xb40>
100deaee0:     	ldrh	w5, [x27, #0xc]
100deaee4:     	ldrh	w6, [x27, #0x10]
100deaee8:     	ldr	w7, [x27, #0x14]
100deaeec:     	ldr	x4, [sp, #0x358]
100deaef0:     	ldr	x8, [x4, #0x10]
100deaef4:     	ldr	x8, [x8, x6, lsl #3]
100deaef8:     	and	x9, x8, #0xfffe000000000000
100deaefc:     	mov	x10, #-0x2000000000000  ; =-562949953421312
100deaf00:     	cmp	x9, x10
100deaf04:     	b.eq	0x100deb628 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1068>
100deaf08:     	ldr	x2, [sp, #0x328]
100deaf0c:     	ldr	w22, [sp, #0x348]
100deaf10:     	cbz	x9, 0x100defe18 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5858>
100deaf14:     	mov	x9, #-0x2000000000000   ; =-562949953421312
100deaf18:     	add	x9, x8, x9
100deaf1c:     	fmov	d0, x9
100deaf20:     	b	0x100deb630 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1070>
100deaf24:     	ldrh	w5, [x27, #0xc]
100deaf28:     	ldrh	w6, [x27, #0x10]
100deaf2c:     	ldr	w7, [x27, #0x14]
100deaf30:     	ldr	x4, [sp, #0x358]
100deaf34:     	ldr	x8, [x4, #0x10]
100deaf38:     	ldr	x8, [x8, x6, lsl #3]
100deaf3c:     	and	x9, x8, #0xfffe000000000000
100deaf40:     	mov	x10, #-0x2000000000000  ; =-562949953421312
100deaf44:     	cmp	x9, x10
100deaf48:     	b.eq	0x100deb690 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x10d0>
100deaf4c:     	cbz	x9, 0x100defe48 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5888>
100deaf50:     	mov	x9, #-0x2000000000000   ; =-562949953421312
100deaf54:     	add	x9, x8, x9
100deaf58:     	ubfx	x10, x9, #52, #11
100deaf5c:     	cmp	x10, #0x7fe
100deaf60:     	ldr	w22, [sp, #0x348]
100deaf64:     	b.ls	0x100dec150 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1b90>
100deaf68:     	mov	w9, #0x0                ; =0
100deaf6c:     	b	0x100dec1ac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1bec>
100deaf70:     	ldr	w10, [x27, #0xc]
100deaf74:     	ldr	x8, [sp, #0x358]
100deaf78:     	ldrh	w11, [x8, #0x8]
100deaf7c:     	ldr	x9, [x8, #0x10]
100deaf80:     	cmp	w11, w10, uxth
100deaf84:     	ccmp	x9, #0x0, #0x4, hi
100deaf88:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100deaf8c:     	and	x10, x10, #0xffff
100deaf90:     	mov	w11, #0xa               ; =10
100deaf94:     	b	0x100deb354 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xd94>
100deaf98:     	ldr	w24, [x27, #0x14]
100deaf9c:     	ldr	x9, [sp, #0x358]
100deafa0:     	ldr	w8, [x9, #0x18]
100deafa4:     	add	w10, w24, #0x1
100deafa8:     	cmp	w8, w10, uxth
100deafac:     	b.ls	0x100df1228 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c68>
100deafb0:     	add	x21, x27, #0xc
100deafb4:     	ldr	x9, [x9, #0x10]
100deafb8:     	and	x10, x10, #0xffff
100deafbc:     	ldr	x15, [x9, x10, lsl #3]
100deafc0:     	cmp	x15, #0x7
100deafc4:     	mov	w10, #0x1               ; =1
100deafc8:     	cinc	w11, w10, ne
100deafcc:     	mov	x10, #-0x2000000000001  ; =-562949953421313
100deafd0:     	cmp	x15, x10
100deafd4:     	csel	w10, wzr, w11, hi
100deafd8:     	cmp	w10, #0x2
100deafdc:     	str	x15, [sp, #0x2d0]
100deafe0:     	b.ne	0x100ded6b0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x30f0>
100deafe4:     	and	x10, x24, #0xffff
100deafe8:     	cmp	w8, w10
100deafec:     	b.ls	0x100df124c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c8c>
100deaff0:     	add	x8, x9, x10, lsl #3
100deaff4:     	ldr	x14, [x8]
100deaff8:     	ldrb	w9, [x27, #0x1d]
100deaffc:     	str	x14, [sp, #0x260]
100deb000:     	cbz	w9, 0x100df0ea0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68e0>
100deb004:     	str	x8, [sp, #0x250]
100deb008:     	add	x8, x27, #0xc
100deb00c:     	cmp	w9, #0x5
100deb010:     	b.lo	0x100deb02c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xa6c>
100deb014:     	ldr	x10, [x25, #0x88]
100deb018:     	ldr	w8, [x21]
100deb01c:     	cmp	x10, x8
100deb020:     	b.ls	0x100df0ea0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68e0>
100deb024:     	ldr	x10, [x25, #0x80]
100deb028:     	add	x8, x10, x8, lsl #2
100deb02c:     	ldr	w16, [x8]
100deb030:     	ldrb	w8, [x27, #0x1c]
100deb034:     	adrp	x10, 0x103403000 <_anon.3ab1adb6df650f9c887f022285db36b4.171+0x3818>
100deb038:     	add	x10, x10, #0x388
100deb03c:     	mov	w11, #0x58              ; =88
100deb040:     	umaddl	x11, w8, w11, x10
100deb044:     	ldrb	w13, [x11]
100deb048:     	ldur	w12, [x11, #0x1]
100deb04c:     	ldr	x10, [x11, #0x10]
100deb050:     	mov	x8, x10
100deb054:     	tbnz	w13, #0x0, 0x100deb060 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xaa0>
100deb058:     	cbz	x10, 0x100df0ea0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68e0>
100deb05c:     	ldr	x8, [x11, #0x8]
100deb060:     	mov	x22, #0x0               ; =0
100deb064:     	mov	w26, #0x1               ; =1
100deb068:     	lsr	w14, w16, #16
100deb06c:     	cbnz	w14, 0x100df0ed0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6910>
100deb070:     	ldrb	w14, [x8]
100deb074:     	mov	w28, #0x1               ; =1
100deb078:     	cbnz	w14, 0x100df1d08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7748>
100deb07c:     	cmp	w9, #0x1
100deb080:     	b.eq	0x100df0ea0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68e0>
100deb084:     	cmp	w9, #0x5
100deb088:     	b.hs	0x100deb894 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x12d4>
100deb08c:     	add	x8, x27, #0x10
100deb090:     	b	0x100deb8b0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x12f0>
100deb094:     	ldrh	w9, [x27, #0x10]
100deb098:     	ldr	x8, [sp, #0x358]
100deb09c:     	ldr	x10, [x8, #0x10]
100deb0a0:     	ldr	x9, [x10, x9, lsl #3]
100deb0a4:     	and	x9, x9, #0xfffffffffffffff7
100deb0a8:     	cmp	x9, #0x2
100deb0ac:     	b.ne	0x100deb4f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xf38>
100deb0b0:     	ldrsw	x10, [x27, #0xc]
100deb0b4:     	ldr	w9, [x8, #0x4]
100deb0b8:     	add	x9, x10, x9
100deb0bc:     	add	x9, x9, #0x1
100deb0c0:     	lsr	x11, x9, #32
100deb0c4:     	ldr	x12, [sp, #0x2f8]
100deb0c8:     	cbnz	x11, 0x100df12b8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6cf8>
100deb0cc:     	ldr	w22, [sp, #0x348]
100deb0d0:     	tbz	w10, #0x1f, 0x100deb0e0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xb20>
100deb0d4:     	ldr	x10, [x20, #0x77f0]
100deb0d8:     	ldapurb	w10, [x10, #0x18]
100deb0dc:     	cbnz	w10, 0x100df143c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e7c>
100deb0e0:     	str	w9, [x8, #0x4]
100deb0e4:     	ldr	x8, [sp, #0x358]
100deb0e8:     	b	0x100dea894 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2d4>
100deb0ec:     	ldrh	w8, [x27, #0xc]
100deb0f0:     	ldr	x9, [sp, #0x358]
100deb0f4:     	ldr	x10, [x9, #0x10]
100deb0f8:     	ldr	x8, [x10, x8, lsl #3]
100deb0fc:     	ldrh	w11, [x27, #0x10]
100deb100:     	str	x8, [x10, x11, lsl #3]
100deb104:     	b	0x100debfa8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x19e8>
100deb108:     	ldr	w10, [x27, #0xc]
100deb10c:     	ldr	x8, [sp, #0x358]
100deb110:     	ldrh	w11, [x8, #0x8]
100deb114:     	ldr	x9, [x8, #0x10]
100deb118:     	cmp	w11, w10, uxth
100deb11c:     	ccmp	x9, #0x0, #0x4, hi
100deb120:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100deb124:     	and	x10, x10, #0xffff
100deb128:     	mov	w11, #0x12              ; =18
100deb12c:     	b	0x100deb354 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xd94>
100deb130:     	ldr	w5, [x27, #0xc]
100deb134:     	ldrb	w8, [x27, #0x1d]
100deb138:     	cmp	w8, #0x3
100deb13c:     	b.lo	0x100deb500 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xf40>
100deb140:     	cmp	w8, #0x5
100deb144:     	ldr	x2, [sp, #0x328]
100deb148:     	ldr	w22, [sp, #0x348]
100deb14c:     	b.hs	0x100debfc0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1a00>
100deb150:     	ldr	w7, [x27, #0x14]
100deb154:     	b	0x100deb50c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xf4c>
100deb158:     	ldrsw	x21, [x27, #0xc]
100deb15c:     	ldrh	w8, [x27, #0x10]
100deb160:     	ldr	x19, [sp, #0x358]
100deb164:     	ldr	x9, [x19, #0x10]
100deb168:     	ldr	x23, [x9, x8, lsl #3]
100deb16c:     	cmp	x23, #0x7
100deb170:     	b.eq	0x100debcac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x16ec>
100deb174:     	cmp	x23, #0x6
100deb178:     	ldr	w22, [sp, #0x348]
100deb17c:     	b.ne	0x100debcc4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1704>
100deb180:     	mov	w0, #0x0                ; =0
100deb184:     	b	0x100debcf8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1738>
100deb188:     	ldrsw	x21, [x27, #0xc]
100deb18c:     	ldrh	w8, [x27, #0x10]
100deb190:     	ldr	x19, [sp, #0x358]
100deb194:     	ldr	x9, [x19, #0x10]
100deb198:     	ldr	x23, [x9, x8, lsl #3]
100deb19c:     	cmp	x23, #0x7
100deb1a0:     	b.eq	0x100debcb8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x16f8>
100deb1a4:     	cmp	x23, #0x6
100deb1a8:     	ldr	w22, [sp, #0x348]
100deb1ac:     	b.ne	0x100debd60 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x17a0>
100deb1b0:     	mov	w0, #0x0                ; =0
100deb1b4:     	b	0x100debd94 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x17d4>
100deb1b8:     	ldrh	w5, [x27, #0xc]
100deb1bc:     	ldrh	w6, [x27, #0x10]
100deb1c0:     	ldr	x4, [sp, #0x358]
100deb1c4:     	ldr	x8, [x4, #0x10]
100deb1c8:     	ldr	x9, [x8, x6, lsl #3]
100deb1cc:     	ldrh	w7, [x27, #0x14]
100deb1d0:     	ldr	x8, [x8, x7, lsl #3]
100deb1d4:     	and	x10, x9, #0xfffe000000000000
100deb1d8:     	mov	x11, #-0x2000000000000  ; =-562949953421312
100deb1dc:     	cmp	x10, x11
100deb1e0:     	b.eq	0x100deb6b0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x10f0>
100deb1e4:     	ldr	x2, [sp, #0x328]
100deb1e8:     	ldr	w22, [sp, #0x348]
100deb1ec:     	cbz	x10, 0x100defaa0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x54e0>
100deb1f0:     	mov	x10, #-0x2000000000000  ; =-562949953421312
100deb1f4:     	add	x10, x9, x10
100deb1f8:     	fmov	d0, x10
100deb1fc:     	mov	w12, #0x1               ; =1
100deb200:     	b	0x100deb6c0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1100>
100deb204:     	ldrsw	x10, [x27, #0xc]
100deb208:     	ldr	x8, [sp, #0x358]
100deb20c:     	ldr	w9, [x8, #0x4]
100deb210:     	add	x9, x10, x9
100deb214:     	add	x9, x9, #0x1
100deb218:     	lsr	x11, x9, #32
100deb21c:     	cbnz	x11, 0x100df121c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c5c>
100deb220:     	tbnz	w10, #0x1f, 0x100deb944 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1384>
100deb224:     	str	w9, [x8, #0x4]
100deb228:     	b	0x100debfb4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x19f4>
100deb22c:     	ldrh	w5, [x27, #0xc]
100deb230:     	ldrh	w6, [x27, #0x10]
100deb234:     	ldr	x4, [sp, #0x358]
100deb238:     	ldr	x8, [x4, #0x10]
100deb23c:     	ldr	x9, [x8, x6, lsl #3]
100deb240:     	ldrh	w7, [x27, #0x14]
100deb244:     	ldr	x8, [x8, x7, lsl #3]
100deb248:     	and	x10, x9, #0xfffe000000000000
100deb24c:     	mov	x11, #-0x2000000000000  ; =-562949953421312
100deb250:     	cmp	x10, x11
100deb254:     	b.eq	0x100deb6f0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1130>
100deb258:     	ldr	x2, [sp, #0x328]
100deb25c:     	ldr	w22, [sp, #0x348]
100deb260:     	cbz	x10, 0x100defae8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5528>
100deb264:     	mov	x10, #-0x2000000000000  ; =-562949953421312
100deb268:     	add	x10, x9, x10
100deb26c:     	fmov	d1, x10
100deb270:     	mov	w13, #0x1               ; =1
100deb274:     	b	0x100deb700 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1140>
100deb278:     	ldrh	w5, [x27, #0xc]
100deb27c:     	ldrh	w6, [x27, #0x10]
100deb280:     	ldr	w7, [x27, #0x14]
100deb284:     	ldr	x4, [sp, #0x358]
100deb288:     	ldr	x8, [x4, #0x10]
100deb28c:     	ldr	x8, [x8, x6, lsl #3]
100deb290:     	and	x9, x8, #0xfffe000000000000
100deb294:     	mov	x10, #-0x2000000000000  ; =-562949953421312
100deb298:     	cmp	x9, x10
100deb29c:     	b.eq	0x100deb728 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1168>
100deb2a0:     	ldr	w22, [sp, #0x348]
100deb2a4:     	cbz	x9, 0x100defdfc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x583c>
100deb2a8:     	mov	x9, #-0x2000000000000   ; =-562949953421312
100deb2ac:     	add	x9, x8, x9
100deb2b0:     	fmov	d0, x9
100deb2b4:     	scvtf	d1, w7
100deb2b8:     	fadd	d0, d0, d1
100deb2bc:     	fcvtzs	w9, d0
100deb2c0:     	scvtf	d1, w9
100deb2c4:     	fcmp	d0, d1
100deb2c8:     	b.ne	0x100ded1a4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2be4>
100deb2cc:     	cbnz	w9, 0x100deb734 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1174>
100deb2d0:     	fmov	x10, d0
100deb2d4:     	tbnz	x10, #0x3f, 0x100ded1a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2be8>
100deb2d8:     	b	0x100deb734 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1174>
100deb2dc:     	ldrh	w5, [x27, #0xc]
100deb2e0:     	ldrh	w6, [x27, #0x10]
100deb2e4:     	ldr	x4, [sp, #0x358]
100deb2e8:     	ldr	x8, [x4, #0x10]
100deb2ec:     	ldr	x9, [x8, x6, lsl #3]
100deb2f0:     	ldrh	w7, [x27, #0x14]
100deb2f4:     	ldr	x8, [x8, x7, lsl #3]
100deb2f8:     	and	x10, x9, #0xfffe000000000000
100deb2fc:     	mov	x11, #-0x2000000000000  ; =-562949953421312
100deb300:     	cmp	x10, x11
100deb304:     	b.eq	0x100deb740 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1180>
100deb308:     	ldr	x2, [sp, #0x328]
100deb30c:     	ldr	w22, [sp, #0x348]
100deb310:     	cbz	x10, 0x100defab8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x54f8>
100deb314:     	mov	x10, #-0x2000000000000  ; =-562949953421312
100deb318:     	add	x10, x9, x10
100deb31c:     	fmov	d0, x10
100deb320:     	mov	w12, #0x1               ; =1
100deb324:     	b	0x100deb750 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1190>
100deb328:     	ldr	x8, [sp, #0x358]
100deb32c:     	b	0x100deb358 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xd98>
100deb330:     	ldr	w10, [x27, #0xc]
100deb334:     	ldr	x8, [sp, #0x358]
100deb338:     	ldrh	w11, [x8, #0x8]
100deb33c:     	ldr	x9, [x8, #0x10]
100deb340:     	cmp	w11, w10, uxth
100deb344:     	ccmp	x9, #0x0, #0x4, hi
100deb348:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100deb34c:     	and	x10, x10, #0xffff
100deb350:     	mov	w11, #0x6               ; =6
100deb354:     	str	x11, [x9, x10, lsl #3]
100deb358:     	ldr	w9, [x8, #0x4]
100deb35c:     	cmn	w9, #0x1
100deb360:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100deb364:     	add	w9, w9, #0x1
100deb368:     	str	w9, [x8, #0x4]
100deb36c:     	b	0x100debfb4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x19f4>
100deb370:     	tbnz	w11, #0x0, 0x100deb384 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xdc4>
100deb374:     	cmp	w27, #0x0
100deb378:     	ccmp	w9, #0x0, #0x0, ne
100deb37c:     	b.eq	0x100dec990 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x23d0>
100deb380:     	scvtf	d0, w21
100deb384:     	scvtf	d2, w27
100deb388:     	cmp	w9, #0x0
100deb38c:     	fcsel	d1, d1, d2, ne
100deb390:     	bl	0x101bb21ec <dyld_stub_binder+0x101bb21ec>
100deb394:     	str	d0, [sp, #0x428]
100deb398:     	mov	w8, #0x1                ; =1
100deb39c:     	str	w8, [sp, #0x420]
100deb3a0:     	cbz	w8, 0x100deb3ac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xdec>
100deb3a4:     	ldr	d0, [sp, #0x428]
100deb3a8:     	b	0x100deb3f4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xe34>
100deb3ac:     	ldr	w8, [sp, #0x424]
100deb3b0:     	mov	w8, w8
100deb3b4:     	b	0x100deb410 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xe50>
100deb3b8:     	tbnz	w11, #0x0, 0x100deb3c4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xe04>
100deb3bc:     	cbz	w9, 0x100dec95c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x239c>
100deb3c0:     	scvtf	d0, w21
100deb3c4:     	scvtf	d2, w27
100deb3c8:     	cmp	w9, #0x0
100deb3cc:     	fcsel	d1, d1, d2, ne
100deb3d0:     	fsub	d0, d0, d1
100deb3d4:     	b	0x100deb3f4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xe34>
100deb3d8:     	scvtf	d2, w21
100deb3dc:     	cmp	w11, #0x0
100deb3e0:     	fcsel	d0, d0, d2, ne
100deb3e4:     	scvtf	d2, w27
100deb3e8:     	cmp	w9, #0x0
100deb3ec:     	fcsel	d1, d1, d2, ne
100deb3f0:     	fdiv	d0, d0, d1
100deb3f4:     	fcvtzs	w8, d0
100deb3f8:     	scvtf	d1, w8
100deb3fc:     	fcmp	d0, d1
100deb400:     	b.ne	0x100deb41c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xe5c>
100deb404:     	cbnz	w8, 0x100deb410 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xe50>
100deb408:     	fmov	x9, d0
100deb40c:     	tbnz	x9, #0x3f, 0x100deb420 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xe60>
100deb410:     	orr	x8, x8, #0xfffe000000000000
100deb414:     	cbnz	x28, 0x100deb438 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xe78>
100deb418:     	b	0x100debf9c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x19dc>
100deb41c:     	fmov	x9, d0
100deb420:     	mov	x8, #0x2000000000000    ; =562949953421312
100deb424:     	add	x8, x9, x8
100deb428:     	fcmp	d0, d0
100deb42c:     	mov	x9, #0x7ffa000000000000 ; =9221683186994511872
100deb430:     	csel	x8, x9, x8, vs
100deb434:     	cbz	x28, 0x100debf9c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x19dc>
100deb438:     	mov	x9, #-0x2000000000001   ; =-562949953421313
100deb43c:     	cmp	x21, x9
100deb440:     	mov	w10, #0x1               ; =1
100deb444:     	cinc	w11, w10, ls
100deb448:     	cmp	x27, x9
100deb44c:     	cinc	w9, w10, ls
100deb450:     	orr	w9, w9, w11
100deb454:     	ldsetb	w9, w10, [x26]
100deb458:     	bics	wzr, w9, w10
100deb45c:     	b.eq	0x100deb488 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xec8>
100deb460:     	ldr	w9, [x28, #0x20]
100deb464:     	cmn	w9, #0x1
100deb468:     	b.eq	0x100deb488 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xec8>
100deb46c:     	add	w10, w9, #0x1
100deb470:     	add	x11, x28, #0x20
100deb474:     	mov	x12, x9
100deb478:     	casl	w12, w10, [x11]
100deb47c:     	cmp	w12, w9
100deb480:     	mov	x9, x12
100deb484:     	b.ne	0x100deb464 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xea4>
100deb488:     	and	x9, x8, #0xfffe000000000000
100deb48c:     	mov	x10, #-0x2000000000000  ; =-562949953421312
100deb490:     	cmp	x9, x10
100deb494:     	b.eq	0x100deb4e0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xf20>
100deb498:     	cbnz	x9, 0x100deb4ec <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xf2c>
100deb49c:     	mov	x10, #0x2               ; =2
100deb4a0:     	movk	x10, #0xfffe, lsl #48
100deb4a4:     	mov	w9, #0x10               ; =16
100deb4a8:     	tst	x8, x10
100deb4ac:     	b.ne	0x100debf14 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1954>
100deb4b0:     	mov	w10, #-0x3              ; =-3
100deb4b4:     	mov	w11, #0x10              ; =16
100deb4b8:     	and	x10, x8, x10
100deb4bc:     	cbz	x10, 0x100debf64 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x19a4>
100deb4c0:     	adrp	x11, 0x1034f4000 <_global_added_lock+0x68>
100deb4c4:     	add	x11, x11, #0xef8
100deb4c8:     	ldr	x9, [x11]
100deb4cc:     	ldrb	w9, [x9, x10]
100deb4d0:     	cmp	w9, #0x20
100deb4d4:     	b.ne	0x100debf1c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x195c>
100deb4d8:     	mov	w9, #0x4                ; =4
100deb4dc:     	b	0x100debf34 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1974>
100deb4e0:     	mov	w9, #0x1                ; =1
100deb4e4:     	mov	w11, #0x1               ; =1
100deb4e8:     	b	0x100debf64 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x19a4>
100deb4ec:     	mov	w9, #0x2                ; =2
100deb4f0:     	mov	w11, #0x2               ; =2
100deb4f4:     	b	0x100debf64 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x19a4>
100deb4f8:     	ldr	w9, [x8, #0x4]
100deb4fc:     	b	0x100deb364 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xda4>
100deb500:     	mov	w7, #0x1                ; =1
100deb504:     	ldr	x2, [sp, #0x328]
100deb508:     	ldr	w22, [sp, #0x348]
100deb50c:     	ldrh	w6, [x27, #0x10]
100deb510:     	ldr	x4, [sp, #0x358]
100deb514:     	ldr	x8, [x4, #0x10]
100deb518:     	ldr	x9, [x8, x6, lsl #3]
100deb51c:     	and	x8, x9, #0xfffe000000000000
100deb520:     	mov	x10, #-0x2000000000000  ; =-562949953421312
100deb524:     	cmp	x8, x10
100deb528:     	b.eq	0x100deb69c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x10dc>
100deb52c:     	cbz	x8, 0x100defe30 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5870>
100deb530:     	mov	x8, #-0x2000000000000   ; =-562949953421312
100deb534:     	add	x8, x9, x8
100deb538:     	fmov	d0, x8
100deb53c:     	scvtf	d1, w7
100deb540:     	fadd	d0, d1, d0
100deb544:     	fcvtzs	w8, d0
100deb548:     	scvtf	d1, w8
100deb54c:     	fcmp	d0, d1
100deb550:     	b.ne	0x100dec9d4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2414>
100deb554:     	cbnz	w8, 0x100deb6a4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x10e4>
100deb558:     	fmov	x10, d0
100deb55c:     	tbnz	x10, #0x3f, 0x100dec9d8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2418>
100deb560:     	b	0x100deb6a4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x10e4>
100deb564:     	mov	w13, #0x0               ; =0
100deb568:     	mov	x10, x9
100deb56c:     	ldr	x2, [sp, #0x328]
100deb570:     	ldr	w22, [sp, #0x348]
100deb574:     	and	x11, x8, #0xfffe000000000000
100deb578:     	mov	x12, #-0x2000000000000  ; =-562949953421312
100deb57c:     	cmp	x11, x12
100deb580:     	b.eq	0x100deb780 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x11c0>
100deb584:     	cbz	x11, 0x100defad0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5510>
100deb588:     	mov	x11, #-0x2000000000000  ; =-562949953421312
100deb58c:     	add	x11, x8, x11
100deb590:     	fmov	d0, x11
100deb594:     	mov	w12, #0x1               ; =1
100deb598:     	b	0x100deb788 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x11c8>
100deb59c:     	mov	w13, #0x0               ; =0
100deb5a0:     	mov	x10, x9
100deb5a4:     	ldr	x2, [sp, #0x328]
100deb5a8:     	ldr	w22, [sp, #0x348]
100deb5ac:     	and	x11, x8, #0xfffe000000000000
100deb5b0:     	mov	x12, #-0x2000000000000  ; =-562949953421312
100deb5b4:     	cmp	x11, x12
100deb5b8:     	b.eq	0x100deb7c8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1208>
100deb5bc:     	cbz	x11, 0x100defb00 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5540>
100deb5c0:     	mov	x11, #-0x2000000000000  ; =-562949953421312
100deb5c4:     	add	x11, x8, x11
100deb5c8:     	fmov	d0, x11
100deb5cc:     	mov	w12, #0x1               ; =1
100deb5d0:     	b	0x100deb7d0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1210>
100deb5d4:     	mov	w8, #0x0                ; =0
100deb5d8:     	ldr	x2, [sp, #0x328]
100deb5dc:     	ldr	w22, [sp, #0x348]
100deb5e0:     	and	x11, x9, #0xfffe000000000000
100deb5e4:     	mov	x12, #-0x2000000000000  ; =-562949953421312
100deb5e8:     	cmp	x11, x12
100deb5ec:     	b.eq	0x100deb810 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1250>
100deb5f0:     	cbz	x11, 0x100defb18 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5558>
100deb5f4:     	mov	x11, #-0x2000000000000  ; =-562949953421312
100deb5f8:     	add	x11, x9, x11
100deb5fc:     	fmov	d1, x11
100deb600:     	scvtf	d2, w10
100deb604:     	cmp	w8, #0x0
100deb608:     	fcsel	d0, d0, d2, ne
100deb60c:     	b	0x100deb818 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1258>
100deb610:     	subs	w9, w8, w7
100deb614:     	ldr	w22, [sp, #0x348]
100deb618:     	b.vs	0x100ded0d8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2b18>
100deb61c:     	orr	x9, x9, #0xfffe000000000000
100deb620:     	cbnz	x28, 0x100ded110 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2b50>
100deb624:     	b	0x100ded204 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c44>
100deb628:     	scvtf	d0, w8
100deb62c:     	ldr	w22, [sp, #0x348]
100deb630:     	scvtf	d1, w7
100deb634:     	fcmp	d0, d1
100deb638:     	mov	w9, #0x6                ; =6
100deb63c:     	mov	w10, #0x7               ; =7
100deb640:     	csel	x9, x10, x9, mi
100deb644:     	cbz	x28, 0x100ded204 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c44>
100deb648:     	mov	x10, #-0x2000000000001  ; =-562949953421313
100deb64c:     	cmp	x8, x10
100deb650:     	mov	w8, #0x3                ; =3
100deb654:     	csinc	w8, w8, wzr, ls
100deb658:     	ldsetb	w8, w10, [x26]
100deb65c:     	bics	wzr, w8, w10
100deb660:     	b.eq	0x100ded204 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c44>
100deb664:     	ldr	w8, [x28, #0x20]
100deb668:     	cmn	w8, #0x1
100deb66c:     	b.eq	0x100ded204 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c44>
100deb670:     	add	w10, w8, #0x1
100deb674:     	add	x11, x28, #0x20
100deb678:     	mov	x12, x8
100deb67c:     	casl	w12, w10, [x11]
100deb680:     	cmp	w12, w8
100deb684:     	mov	x8, x12
100deb688:     	b.ne	0x100deb668 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x10a8>
100deb68c:     	b	0x100ded204 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c44>
100deb690:     	mov	x9, x8
100deb694:     	ldr	w22, [sp, #0x348]
100deb698:     	b	0x100dec1ac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1bec>
100deb69c:     	adds	w8, w9, w7
100deb6a0:     	b.vs	0x100ded158 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2b98>
100deb6a4:     	orr	x8, x8, #0xfffe000000000000
100deb6a8:     	cbnz	x28, 0x100dec9f0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2430>
100deb6ac:     	b	0x100decc04 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2644>
100deb6b0:     	mov	w12, #0x0               ; =0
100deb6b4:     	mov	x10, x9
100deb6b8:     	ldr	x2, [sp, #0x328]
100deb6bc:     	ldr	w22, [sp, #0x348]
100deb6c0:     	and	x11, x8, #0xfffe000000000000
100deb6c4:     	mov	x13, #-0x2000000000000  ; =-562949953421312
100deb6c8:     	cmp	x11, x13
100deb6cc:     	b.eq	0x100deb83c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x127c>
100deb6d0:     	cbz	x11, 0x100defaa0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x54e0>
100deb6d4:     	mov	x11, #-0x2000000000000  ; =-562949953421312
100deb6d8:     	add	x11, x8, x11
100deb6dc:     	ubfx	x13, x11, #52, #11
100deb6e0:     	cmp	x13, #0x7fe
100deb6e4:     	b.ls	0x100dec550 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1f90>
100deb6e8:     	mov	w11, #0x0               ; =0
100deb6ec:     	b	0x100dec5ac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1fec>
100deb6f0:     	mov	w13, #0x0               ; =0
100deb6f4:     	mov	x10, x9
100deb6f8:     	ldr	x2, [sp, #0x328]
100deb6fc:     	ldr	w22, [sp, #0x348]
100deb700:     	and	x11, x8, #0xfffe000000000000
100deb704:     	mov	x12, #-0x2000000000000  ; =-562949953421312
100deb708:     	cmp	x11, x12
100deb70c:     	b.eq	0x100deb844 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1284>
100deb710:     	cbz	x11, 0x100defae8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5528>
100deb714:     	mov	x11, #-0x2000000000000  ; =-562949953421312
100deb718:     	add	x11, x8, x11
100deb71c:     	fmov	d0, x11
100deb720:     	mov	w12, #0x1               ; =1
100deb724:     	b	0x100deb84c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x128c>
100deb728:     	adds	w9, w8, w7
100deb72c:     	ldr	w22, [sp, #0x348]
100deb730:     	b.vs	0x100ded188 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2bc8>
100deb734:     	orr	x9, x9, #0xfffe000000000000
100deb738:     	cbnz	x28, 0x100ded1c0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c00>
100deb73c:     	b	0x100ded204 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c44>
100deb740:     	mov	w12, #0x0               ; =0
100deb744:     	mov	x10, x9
100deb748:     	ldr	x2, [sp, #0x328]
100deb74c:     	ldr	w22, [sp, #0x348]
100deb750:     	and	x11, x8, #0xfffe000000000000
100deb754:     	mov	x13, #-0x2000000000000  ; =-562949953421312
100deb758:     	cmp	x11, x13
100deb75c:     	b.eq	0x100deb88c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x12cc>
100deb760:     	cbz	x11, 0x100defab8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x54f8>
100deb764:     	mov	x11, #-0x2000000000000  ; =-562949953421312
100deb768:     	add	x11, x8, x11
100deb76c:     	ubfx	x13, x11, #52, #11
100deb770:     	cmp	x13, #0x7fe
100deb774:     	b.ls	0x100dec7fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x223c>
100deb778:     	mov	w11, #0x0               ; =0
100deb77c:     	b	0x100dec858 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2298>
100deb780:     	mov	w12, #0x0               ; =0
100deb784:     	mov	x11, x8
100deb788:     	cbz	w13, 0x100deb7a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x11e8>
100deb78c:     	fmov	x10, d1
100deb790:     	ubfx	x10, x10, #52, #11
100deb794:     	cmp	x10, #0x7ff
100deb798:     	b.lo	0x100dec200 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1c40>
100deb79c:     	mov	w10, #0x0               ; =0
100deb7a0:     	tbz	w12, #0x0, 0x100dec25c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1c9c>
100deb7a4:     	b	0x100deb7ac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x11ec>
100deb7a8:     	tbz	w12, #0x0, 0x100dec25c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1c9c>
100deb7ac:     	fmov	x11, d0
100deb7b0:     	ubfx	x11, x11, #52, #11
100deb7b4:     	cmp	x11, #0x7fe
100deb7b8:     	b.ls	0x100dec2e4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1d24>
100deb7bc:     	mov	w11, #0x0               ; =0
100deb7c0:     	cbnz	x28, 0x100dec260 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1ca0>
100deb7c4:     	b	0x100dec340 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1d80>
100deb7c8:     	mov	w12, #0x0               ; =0
100deb7cc:     	mov	x11, x8
100deb7d0:     	cbz	w13, 0x100deb7f0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1230>
100deb7d4:     	fmov	x10, d1
100deb7d8:     	ubfx	x10, x10, #52, #11
100deb7dc:     	cmp	x10, #0x7ff
100deb7e0:     	b.lo	0x100dec34c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1d8c>
100deb7e4:     	mov	w10, #0x0               ; =0
100deb7e8:     	tbz	w12, #0x0, 0x100dec3a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1de8>
100deb7ec:     	b	0x100deb7f4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1234>
100deb7f0:     	tbz	w12, #0x0, 0x100dec3a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1de8>
100deb7f4:     	fmov	x11, d0
100deb7f8:     	ubfx	x11, x11, #52, #11
100deb7fc:     	cmp	x11, #0x7fe
100deb800:     	b.ls	0x100dec430 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1e70>
100deb804:     	mov	w11, #0x0               ; =0
100deb808:     	cbnz	x28, 0x100dec3ac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1dec>
100deb80c:     	b	0x100dec48c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1ecc>
100deb810:     	cbz	w8, 0x100dec498 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1ed8>
100deb814:     	scvtf	d1, w9
100deb818:     	fadd	d0, d0, d1
100deb81c:     	fcvtzs	w8, d0
100deb820:     	scvtf	d1, w8
100deb824:     	fcmp	d0, d1
100deb828:     	b.ne	0x100debef4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1934>
100deb82c:     	cbnz	w8, 0x100dec4a0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1ee0>
100deb830:     	fmov	x11, d0
100deb834:     	tbnz	x11, #0x3f, 0x100debef8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1938>
100deb838:     	b	0x100dec4a0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1ee0>
100deb83c:     	mov	x11, x8
100deb840:     	b	0x100dec5ac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1fec>
100deb844:     	mov	w12, #0x0               ; =0
100deb848:     	mov	x11, x8
100deb84c:     	cbz	w13, 0x100deb86c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x12ac>
100deb850:     	fmov	x10, d1
100deb854:     	ubfx	x10, x10, #52, #11
100deb858:     	cmp	x10, #0x7ff
100deb85c:     	b.lo	0x100dec6b0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x20f0>
100deb860:     	mov	w10, #0x0               ; =0
100deb864:     	tbz	w12, #0x0, 0x100dec70c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x214c>
100deb868:     	b	0x100deb870 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x12b0>
100deb86c:     	tbz	w12, #0x0, 0x100dec70c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x214c>
100deb870:     	fmov	x11, d0
100deb874:     	ubfx	x11, x11, #52, #11
100deb878:     	cmp	x11, #0x7fe
100deb87c:     	b.ls	0x100dec794 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x21d4>
100deb880:     	mov	w11, #0x0               ; =0
100deb884:     	cbnz	x28, 0x100dec710 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2150>
100deb888:     	b	0x100dec7f0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2230>
100deb88c:     	mov	x11, x8
100deb890:     	b	0x100dec858 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2298>
100deb894:     	ldr	x14, [x25, #0x88]
100deb898:     	ldr	w8, [x21]
100deb89c:     	add	x8, x8, #0x1
100deb8a0:     	cmp	x8, x14
100deb8a4:     	b.hs	0x100df0ea0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68e0>
100deb8a8:     	ldr	x14, [x25, #0x80]
100deb8ac:     	add	x8, x14, x8, lsl #2
100deb8b0:     	ldr	w15, [x8]
100deb8b4:     	tbz	w13, #0x0, 0x100deb8d0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1310>
100deb8b8:     	ldr	x14, [x11, #0x18]
100deb8bc:     	mov	x8, x10
100deb8c0:     	cmp	x14, #0x1
100deb8c4:     	b.hi	0x100deb8dc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x131c>
100deb8c8:     	and	w14, w12, #0xff
100deb8cc:     	b	0x100deb8e0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1320>
100deb8d0:     	cmp	x10, #0x2
100deb8d4:     	b.lo	0x100df0ea0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68e0>
100deb8d8:     	ldr	x8, [x11, #0x8]
100deb8dc:     	ldrb	w14, [x8, #0x4]
100deb8e0:     	mov	x22, #0x0               ; =0
100deb8e4:     	mov	w26, #0x1               ; =1
100deb8e8:     	lsr	w8, w15, #16
100deb8ec:     	cbnz	w8, 0x100df0ed0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6910>
100deb8f0:     	str	x15, [sp, #0x270]
100deb8f4:     	mov	w28, #0x1               ; =1
100deb8f8:     	cbnz	w14, 0x100df1d08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7748>
100deb8fc:     	cmp	w9, #0x3
100deb900:     	b.lo	0x100df0ea0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68e0>
100deb904:     	mov	x19, x24
100deb908:     	cmp	w9, #0x5
100deb90c:     	b.lo	0x100deb92c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x136c>
100deb910:     	ldr	x9, [x25, #0x88]
100deb914:     	ldr	w8, [x21]
100deb918:     	add	x8, x8, #0x2
100deb91c:     	cmp	x8, x9
100deb920:     	b.hs	0x100df0ea0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68e0>
100deb924:     	ldr	x9, [x25, #0x80]
100deb928:     	ldr	w19, [x9, x8, lsl #2]
100deb92c:     	tbz	w13, #0x0, 0x100deb9c0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1400>
100deb930:     	ldr	x8, [x11, #0x18]
100deb934:     	cmp	x8, #0x2
100deb938:     	b.hi	0x100deb9cc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x140c>
100deb93c:     	and	w9, w12, #0xff
100deb940:     	b	0x100deb9d0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1410>
100deb944:     	ldr	x10, [x20, #0x77f0]
100deb948:     	ldapurb	w10, [x10, #0x18]
100deb94c:     	ldr	x11, [sp, #0x2f8]
100deb950:     	ldr	w22, [sp, #0x348]
100deb954:     	cbnz	w10, 0x100df1378 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6db8>
100deb958:     	str	w9, [x8, #0x4]
100deb95c:     	ldr	x8, [sp, #0x340]
100deb960:     	cbz	x8, 0x100debfb8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x19f8>
100deb964:     	ldr	x10, [sp, #0x338]
100deb968:     	cbz	x10, 0x100deb980 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x13c0>
100deb96c:     	ldr	x9, [sp, #0x2e0]
100deb970:     	ldr	x8, [x9, #0x10]
100deb974:     	adds	x8, x8, x10
100deb978:     	csinv	x8, x8, xzr, lo
100deb97c:     	str	x8, [x9, #0x10]
100deb980:     	ldr	x4, [sp, #0x360]
100deb984:     	str	xzr, [sp, #0x338]
100deb988:     	add	x0, sp, #0x420
100deb98c:     	mov	x1, x20
100deb990:     	ldr	x2, [sp, #0x328]
100deb994:     	mov	x3, x19
100deb998:     	bl	0x100da521c <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp8jit_callNtB6_11Interpreter27note_backedge_and_maybe_osr>
100deb99c:     	ldrb	w9, [sp, #0x420]
100deb9a0:     	cmp	w9, #0xff
100deb9a4:     	b.ne	0x100df12c0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6d00>
100deb9a8:     	ldr	x8, [sp, #0x428]
100deb9ac:     	ldr	x19, [sp, #0x308]
100deb9b0:     	cbnz	x8, 0x100df12f4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6d34>
100deb9b4:     	str	xzr, [sp, #0x338]
100deb9b8:     	ldr	x8, [sp, #0x358]
100deb9bc:     	b	0x100dea894 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2d4>
100deb9c0:     	cmp	x10, #0x3
100deb9c4:     	b.lo	0x100df0ea0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68e0>
100deb9c8:     	ldr	x10, [x11, #0x8]
100deb9cc:     	ldrb	w9, [x10, #0x8]
100deb9d0:     	mov	x22, #0x0               ; =0
100deb9d4:     	mov	w26, #0x1               ; =1
100deb9d8:     	lsr	w8, w19, #16
100deb9dc:     	cbnz	w8, 0x100df0ed0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6910>
100deb9e0:     	mov	w28, #0x1               ; =1
100deb9e4:     	cbnz	w9, 0x100df1d08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7748>
100deb9e8:     	ldr	x9, [sp, #0x328]
100deb9ec:     	ldr	x8, [x9, #0x180]
100deb9f0:     	str	x16, [sp, #0x268]
100deb9f4:     	cbz	x8, 0x100deba24 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1464>
100deb9f8:     	ldr	x0, [x8, #0x8]
100deb9fc:     	cbz	x0, 0x100deba24 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1464>
100deba00:     	ldrb	w8, [x0, #0xb]
100deba04:     	tbz	w8, #0x1, 0x100deba10 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1450>
100deba08:     	ldr	x0, [x0, #0x48]
100deba0c:     	cbz	x0, 0x100deba24 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1464>
100deba10:     	ldrb	w8, [x0, #0xa]
100deba14:     	cmp	w8, #0x3
100deba18:     	b.ne	0x100decb3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x257c>
100deba1c:     	ldr	x0, [x0, #0x48]
100deba20:     	cbnz	x0, 0x100deba10 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1450>
100deba24:     	mov	x28, #-0x1              ; =-1
100deba28:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100deba2c:     	add	x2, x2, #0x320
100deba30:     	mov	x0, x9
100deba34:     	mov	x1, x28
100deba38:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100deba3c:     	ldr	w26, [x0, #0x4]
100deba40:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100deba44:     	add	x2, x2, #0x338
100deba48:     	ldr	x0, [sp, #0x328]
100deba4c:     	mov	x1, x28
100deba50:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100deba54:     	ldr	x8, [sp, #0x270]
100deba58:     	and	w11, w8, #0xffff
100deba5c:     	ldr	w8, [x0, #0x64]
100deba60:     	ldr	x0, [sp, #0x328]
100deba64:     	str	x28, [sp, #0x348]
100deba68:     	cbz	w8, 0x100debb28 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1568>
100deba6c:     	ldr	x1, [x20, #0x7558]
100deba70:     	sub	x23, x8, #0x1
100deba74:     	cmp	x23, x1
100deba78:     	b.hs	0x100df1e84 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x78c4>
100deba7c:     	ldr	x8, [x20, #0x7550]
100deba80:     	mov	w9, #0xd0               ; =208
100deba84:     	madd	x8, x23, x9, x8
100deba88:     	ldr	x9, [x8]
100deba8c:     	cbz	x9, 0x100debb28 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1568>
100deba90:     	ldr	w9, [x8, #0x10]
100deba94:     	cmp	w9, w26
100deba98:     	b.ne	0x100debb28 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1568>
100deba9c:     	ldrh	w9, [x8, #0x14]
100debaa0:     	ldr	x10, [sp, #0x268]
100debaa4:     	cmp	w9, w10, uxth
100debaa8:     	b.ne	0x100debb28 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1568>
100debaac:     	ldrh	w9, [x8, #0x16]
100debab0:     	cmp	w9, w11
100debab4:     	b.ne	0x100debb28 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1568>
100debab8:     	mov	x27, x11
100debabc:     	ldr	x23, [x8, #0x8]
100debac0:     	ldrh	w19, [x8, #0x18]
100debac4:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100debac8:     	add	x2, x2, #0x458
100debacc:     	mov	x1, x28
100debad0:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100debad4:     	ldr	w8, [x0, #0x18]
100debad8:     	cmp	w8, w19
100debadc:     	b.ls	0x100df0ea0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68e0>
100debae0:     	ldr	x8, [x0, #0x10]
100debae4:     	ldr	x24, [x8, w19, uxtw #3]
100debae8:     	mov	x0, x24
100debaec:     	bl	0x100f0e970 <__RNvNtCsgTc3P8JF8aG_8otter_vm7reflect20is_type_object_value>
100debaf0:     	ldr	x2, [sp, #0x328]
100debaf4:     	tbz	w0, #0x0, 0x100df1b44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7584>
100debaf8:     	sub	x0, x29, #0xa8
100debafc:     	mov	x1, x20
100debb00:     	ldr	x3, [sp, #0x308]
100debb04:     	mov	x4, x24
100debb08:     	bl	0x100e6b99c <__RNvNtCsgTc3P8JF8aG_8otter_vm12iterator_ops18iterator_step_read>
100debb0c:     	ldur	x26, [x29, #-0xa8]
100debb10:     	cmp	x26, #0x2
100debb14:     	b.ne	0x100df1b9c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x75dc>
100debb18:     	ldur	x19, [x29, #-0xa0]
100debb1c:     	tbz	w19, #0x0, 0x100deced4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2914>
100debb20:     	ldur	x21, [x29, #-0x98]
100debb24:     	b	0x100decf20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2960>
100debb28:     	str	w11, [sp, #0x258]
100debb2c:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100debb30:     	add	x2, x2, #0x350
100debb34:     	mov	x1, x28
100debb38:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100debb3c:     	ldr	w8, [x0, #0x18]
100debb40:     	cmp	w8, w19
100debb44:     	b.ls	0x100df0ea0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68e0>
100debb48:     	mov	x21, #0x2               ; =2
100debb4c:     	movk	x21, #0xfffe, lsl #48
100debb50:     	ldr	x8, [x0, #0x10]
100debb54:     	ldr	x23, [x8, w19, uxtw #3]
100debb58:     	and	x8, x23, x21
100debb5c:     	cmp	x23, #0x0
100debb60:     	ccmp	x8, #0x0, #0x0, ne
100debb64:     	b.ne	0x100df1b8c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x75cc>
100debb68:     	cbz	w23, 0x100df1b8c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x75cc>
100debb6c:     	adrp	x12, 0x1034f4000 <_global_added_lock+0x68>
100debb70:     	add	x12, x12, #0xef8
100debb74:     	mov	w19, #-0x3              ; =-3
100debb78:     	ldr	x8, [x12]
100debb7c:     	and	x22, x23, x19
100debb80:     	ldrb	w8, [x8, x22]
100debb84:     	cmp	w8, #0x24
100debb88:     	b.ne	0x100df1b8c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x75cc>
100debb8c:     	stur	w23, [x29, #-0x84]
100debb90:     	ldr	x8, [x12]
100debb94:     	add	x8, x8, x22
100debb98:     	ldr	w9, [x8, #0x8]
100debb9c:     	cmp	w9, #0x11
100debba0:     	b.ne	0x100debdd4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1814>
100debba4:     	mov	x27, x12
100debba8:     	ldr	w8, [x8, #0xc]
100debbac:     	stur	w8, [x29, #-0x78]
100debbb0:     	add	x8, sp, #0x420
100debbb4:     	sub	x3, x29, #0x78
100debbb8:     	mov	x0, x20
100debbbc:     	ldr	x1, [sp, #0x328]
100debbc0:     	ldr	x2, [sp, #0x308]
100debbc4:     	mov	x4, #0x0                ; =0
100debbc8:     	mov	w5, #0xa                ; =10
100debbcc:     	bl	0x100ce6714 <__RNvMNtCsgTc3P8JF8aG_8otter_vm12iterator_opsNtB4_11Interpreter16resume_generator>
100debbd0:     	ldr	x26, [sp, #0x420]
100debbd4:     	ldr	x24, [sp, #0x428]
100debbd8:     	cmp	x26, #0x2
100debbdc:     	b.ne	0x100df138c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6dcc>
100debbe0:     	mov	x22, #0x0               ; =0
100debbe4:     	mov	w28, #0x2               ; =2
100debbe8:     	mov	x26, #0x0               ; =0
100debbec:     	cbz	x24, 0x100df1d00 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7740>
100debbf0:     	and	x9, x24, x21
100debbf4:     	cbnz	x9, 0x100df1d00 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7740>
100debbf8:     	cbz	w24, 0x100df1280 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6cc0>
100debbfc:     	ldr	x8, [x27]
100debc00:     	and	x9, x24, x19
100debc04:     	ldrb	w8, [x8, x9]
100debc08:     	cmp	w8, #0x11
100debc0c:     	b.ne	0x100df1280 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6cc0>
100debc10:     	adrp	x2, 0x102d0e000 <_anon.49087316163ed4345739c0711faf6653.2247+0x2889>
100debc14:     	add	x2, x2, #0x311
100debc18:     	add	x1, x20, #0x430
100debc1c:     	mov	x0, x24
100debc20:     	mov	w3, #0x5                ; =5
100debc24:     	bl	0x100f04278 <__RNvNtCsgTc3P8JF8aG_8otter_vm6object3get>
100debc28:     	ldr	x22, [sp, #0x348]
100debc2c:     	tst	w0, #0x1
100debc30:     	mov	w8, #0xa                ; =10
100debc34:     	csel	x19, x1, x8, ne
100debc38:     	adrp	x2, 0x102367000 <_anon.3ab1adb6df650f9c887f022285db36b4.164+0x7591>
100debc3c:     	add	x2, x2, #0xb74
100debc40:     	add	x1, x20, #0x430
100debc44:     	mov	x0, x24
100debc48:     	mov	w3, #0x4                ; =4
100debc4c:     	bl	0x100f04278 <__RNvNtCsgTc3P8JF8aG_8otter_vm6object3get>
100debc50:     	tst	w0, #0x1
100debc54:     	mov	w8, #0xa                ; =10
100debc58:     	csel	x24, x1, x8, ne
100debc5c:     	cmp	x24, #0x6
100debc60:     	b.eq	0x100deccc4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2704>
100debc64:     	cmp	x24, #0x7
100debc68:     	b.eq	0x100debc9c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x16dc>
100debc6c:     	cbz	x24, 0x100debc78 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x16b8>
100debc70:     	and	x8, x24, x21
100debc74:     	cbz	x8, 0x100debc90 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x16d0>
100debc78:     	mov	x0, x24
100debc7c:     	bl	0x100d255a8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm5valueNtB2_5Value15to_boolean_pure>
100debc80:     	ands	w8, w0, #0xff
100debc84:     	b.eq	0x100deccc0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2700>
100debc88:     	cmp	w8, #0x2
100debc8c:     	b.ne	0x100debc9c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x16dc>
100debc90:     	mov	x0, x24
100debc94:     	bl	0x100d253e8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm5valueNtB2_5Value15to_boolean_cell>
100debc98:     	tbz	w0, #0x0, 0x100deccc0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2700>
100debc9c:     	mov	x0, x23
100debca0:     	bl	0x100c11200 <__RINvMs0_NtCseKsKcgsFYGN_8otter_gc4heapNtB6_6GcHeap12with_payloadNtNtCsgTc3P8JF8aG_8otter_vm14iterator_state13IteratorStateuNCINvMNtB14_12iterator_opsNtB14_11Interpreter19drive_iterator_nextNtNtB14_10executable11OperandViewEs0_0EB14_>
100debca4:     	mov	w24, #0x7               ; =7
100debca8:     	b	0x100deccc4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2704>
100debcac:     	mov	w0, #0x1                ; =1
100debcb0:     	ldr	w22, [sp, #0x348]
100debcb4:     	b	0x100debcf8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1738>
100debcb8:     	mov	w0, #0x1                ; =1
100debcbc:     	ldr	w22, [sp, #0x348]
100debcc0:     	b	0x100debd94 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x17d4>
100debcc4:     	mov	x8, #0x2                ; =2
100debcc8:     	movk	x8, #0xfffe, lsl #48
100debccc:     	and	x8, x23, x8
100debcd0:     	cmp	x23, #0x0
100debcd4:     	ccmp	x8, #0x0, #0x0, ne
100debcd8:     	b.eq	0x100debcf0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1730>
100debcdc:     	mov	x0, x23
100debce0:     	bl	0x100d255a8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm5valueNtB2_5Value15to_boolean_pure>
100debce4:     	and	w0, w0, #0xff
100debce8:     	cmp	w0, #0x2
100debcec:     	b.ne	0x100debcf8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1738>
100debcf0:     	mov	x0, x23
100debcf4:     	bl	0x100d253e8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm5valueNtB2_5Value15to_boolean_cell>
100debcf8:     	ldr	x8, [sp, #0x340]
100debcfc:     	cbz	x8, 0x100dec018 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1a58>
100debd00:     	cbz	x28, 0x100dec018 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1a58>
100debd04:     	ldrb	w8, [x26, #0x3]
100debd08:     	and	w9, w8, #0xff
100debd0c:     	add	w10, w9, #0x1
100debd10:     	cmp	w10, #0xff
100debd14:     	mov	w10, #0xff              ; =255
100debd18:     	csinc	w10, w10, w9, hs
100debd1c:     	add	x11, x26, #0x3
100debd20:     	casb	w8, w10, [x11]
100debd24:     	cmp	w8, w9
100debd28:     	b.ne	0x100debd08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1748>
100debd2c:     	tbz	w0, #0x0, 0x100debfdc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1a1c>
100debd30:     	ldrb	w8, [x26, #0x2]
100debd34:     	and	w9, w8, #0xff
100debd38:     	add	w10, w9, #0x1
100debd3c:     	cmp	w10, #0xff
100debd40:     	mov	w10, #0xff              ; =255
100debd44:     	csinc	w10, w10, w9, hs
100debd48:     	add	x11, x26, #0x2
100debd4c:     	casb	w8, w10, [x11]
100debd50:     	cmp	w8, w9
100debd54:     	b.ne	0x100debd34 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1774>
100debd58:     	mov	w8, #0x20               ; =32
100debd5c:     	b	0x100debfe0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1a20>
100debd60:     	mov	x8, #0x2                ; =2
100debd64:     	movk	x8, #0xfffe, lsl #48
100debd68:     	and	x8, x23, x8
100debd6c:     	cmp	x23, #0x0
100debd70:     	ccmp	x8, #0x0, #0x0, ne
100debd74:     	b.eq	0x100debd8c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x17cc>
100debd78:     	mov	x0, x23
100debd7c:     	bl	0x100d255a8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm5valueNtB2_5Value15to_boolean_pure>
100debd80:     	and	w0, w0, #0xff
100debd84:     	cmp	w0, #0x2
100debd88:     	b.ne	0x100debd94 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x17d4>
100debd8c:     	mov	x0, x23
100debd90:     	bl	0x100d253e8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm5valueNtB2_5Value15to_boolean_cell>
100debd94:     	ldr	x8, [sp, #0x340]
100debd98:     	cbz	x8, 0x100dec10c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1b4c>
100debd9c:     	cbz	x28, 0x100dec10c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1b4c>
100debda0:     	ldrb	w8, [x26, #0x3]
100debda4:     	and	w9, w8, #0xff
100debda8:     	add	w10, w9, #0x1
100debdac:     	cmp	w10, #0xff
100debdb0:     	mov	w10, #0xff              ; =255
100debdb4:     	csinc	w10, w10, w9, hs
100debdb8:     	add	x11, x26, #0x3
100debdbc:     	casb	w8, w10, [x11]
100debdc0:     	cmp	w8, w9
100debdc4:     	b.ne	0x100debda4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x17e4>
100debdc8:     	tbz	w0, #0x0, 0x100dec0a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1ae8>
100debdcc:     	mov	w8, #0x40               ; =64
100debdd0:     	b	0x100dec0d4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1b14>
100debdd4:     	ldr	x8, [x12]
100debdd8:     	add	x8, x8, x22
100debddc:     	ldr	w9, [x8, #0x8]
100debde0:     	sub	w10, w9, #0x2
100debde4:     	mov	w8, #0x10               ; =16
100debde8:     	cmp	w9, #0x1
100debdec:     	csel	w9, w10, w8, hi
100debdf0:     	cmp	w9, #0x14
100debdf4:     	b.hi	0x100decd7c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x27bc>
100debdf8:     	mov	w10, #0x7848            ; =30792
100debdfc:     	movk	w10, #0x1f, lsl #16
100debe00:     	lsr	w9, w10, w9
100debe04:     	tbz	w9, #0x0, 0x100decd7c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x27bc>
100debe08:     	add	x0, sp, #0x420
100debe0c:     	sub	x4, x29, #0x84
100debe10:     	mov	x1, x20
100debe14:     	ldr	x2, [sp, #0x308]
100debe18:     	ldr	x3, [sp, #0x328]
100debe1c:     	bl	0x100ce8880 <__RNvMNtCsgTc3P8JF8aG_8otter_vm12iterator_opsNtB4_11Interpreter18iterator_next_full>
100debe20:     	ldr	x26, [sp, #0x420]
100debe24:     	ldr	x19, [sp, #0x428]
100debe28:     	ldrb	w23, [sp, #0x430]
100debe2c:     	cmp	x26, #0x2
100debe30:     	b.ne	0x100df13bc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6dfc>
100debe34:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100debe38:     	add	x2, x2, #0x410
100debe3c:     	ldr	x0, [sp, #0x328]
100debe40:     	mov	x1, x28
100debe44:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100debe48:     	mov	x22, #0x0               ; =0
100debe4c:     	ldrh	w8, [x0, #0x8]
100debe50:     	mov	w28, #0x1               ; =1
100debe54:     	ldr	x10, [sp, #0x268]
100debe58:     	cmp	w8, w10, uxth
100debe5c:     	b.ls	0x100df1ce8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7728>
100debe60:     	ldr	x9, [x0, #0x10]
100debe64:     	mov	w26, #0x1               ; =1
100debe68:     	ldr	x0, [sp, #0x328]
100debe6c:     	cbz	x9, 0x100df13e0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e20>
100debe70:     	str	x19, [x9, x10, lsl #3]
100debe74:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100debe78:     	add	x2, x2, #0x428
100debe7c:     	ldr	x1, [sp, #0x348]
100debe80:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100debe84:     	mov	x22, #0x0               ; =0
100debe88:     	ldrh	w8, [x0, #0x8]
100debe8c:     	mov	w28, #0x1               ; =1
100debe90:     	ldr	w9, [sp, #0x258]
100debe94:     	cmp	w8, w9
100debe98:     	b.ls	0x100df1ce8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7728>
100debe9c:     	ldr	x9, [x0, #0x10]
100debea0:     	mov	w26, #0x1               ; =1
100debea4:     	ldr	x0, [sp, #0x328]
100debea8:     	cbz	x9, 0x100df13e0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e20>
100debeac:     	tst	w23, #0x1
100debeb0:     	mov	w8, #0x6                ; =6
100debeb4:     	cinc	x8, x8, ne
100debeb8:     	ldr	x10, [sp, #0x270]
100debebc:     	str	x8, [x9, x10, lsl #3]
100debec0:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100debec4:     	add	x2, x2, #0x440
100debec8:     	ldr	x1, [sp, #0x348]
100debecc:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100debed0:     	ldr	w8, [x0, #0x4]
100debed4:     	cmn	w8, #0x1
100debed8:     	b.eq	0x100df1ce0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7720>
100debedc:     	add	w8, w8, #0x1
100debee0:     	str	w8, [x0, #0x4]
100debee4:     	ldr	x19, [sp, #0x308]
100debee8:     	mov	w22, #0x2               ; =2
100debeec:     	ldr	x8, [sp, #0x358]
100debef0:     	b	0x100dea894 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2d4>
100debef4:     	fmov	x11, d0
100debef8:     	mov	x8, #0x2000000000000    ; =562949953421312
100debefc:     	add	x8, x11, x8
100debf00:     	fcmp	d0, d0
100debf04:     	mov	x11, #0x7ffa000000000000 ; =9221683186994511872
100debf08:     	csel	x8, x11, x8, vs
100debf0c:     	cbnz	x28, 0x100dec4a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1ee8>
100debf10:     	b	0x100decca0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x26e0>
100debf14:     	mov	w11, #0x10              ; =16
100debf18:     	b	0x100debf64 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x19a4>
100debf1c:     	ldr	x9, [x11]
100debf20:     	ldrb	w9, [x9, x10]
100debf24:     	mov	w12, #0x10              ; =16
100debf28:     	mov	w13, #0x8               ; =8
100debf2c:     	cmp	w9, #0x25
100debf30:     	csel	w9, w13, w12, eq
100debf34:     	ldr	x12, [x11]
100debf38:     	ldrb	w12, [x12, x10]
100debf3c:     	cmp	w12, #0x20
100debf40:     	b.ne	0x100debf4c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x198c>
100debf44:     	mov	w11, #0x4               ; =4
100debf48:     	b	0x100debf64 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x19a4>
100debf4c:     	ldr	x11, [x11]
100debf50:     	ldrb	w10, [x11, x10]
100debf54:     	mov	w11, #0x10              ; =16
100debf58:     	mov	w12, #0x8               ; =8
100debf5c:     	cmp	w10, #0x25
100debf60:     	csel	w11, w12, w11, eq
100debf64:     	orr	w9, w11, w9
100debf68:     	ldsetb	w9, w10, [x26]
100debf6c:     	bics	wzr, w9, w10
100debf70:     	b.eq	0x100debf9c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x19dc>
100debf74:     	ldr	w9, [x28, #0x20]
100debf78:     	cmn	w9, #0x1
100debf7c:     	b.eq	0x100debf9c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x19dc>
100debf80:     	add	w10, w9, #0x1
100debf84:     	add	x11, x28, #0x20
100debf88:     	mov	x12, x9
100debf8c:     	casl	w12, w10, [x11]
100debf90:     	cmp	w12, w9
100debf94:     	mov	x9, x12
100debf98:     	b.ne	0x100debf78 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x19b8>
100debf9c:     	ldr	x9, [sp, #0x358]
100debfa0:     	ldr	x10, [x9, #0x10]
100debfa4:     	str	x8, [x10, x22, lsl #3]
100debfa8:     	ldr	w8, [x9, #0x4]
100debfac:     	add	w8, w8, #0x1
100debfb0:     	str	w8, [x9, #0x4]
100debfb4:     	ldr	w22, [sp, #0x348]
100debfb8:     	ldr	x8, [sp, #0x358]
100debfbc:     	b	0x100dea894 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2d4>
100debfc0:     	ldr	x9, [x25, #0x88]
100debfc4:     	add	x8, x5, #0x2
100debfc8:     	cmp	x8, x9
100debfcc:     	b.hs	0x100decb18 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2558>
100debfd0:     	ldr	x9, [x25, #0x80]
100debfd4:     	ldr	w7, [x9, x8, lsl #2]
100debfd8:     	b	0x100deb50c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xf4c>
100debfdc:     	mov	w8, #0x40               ; =64
100debfe0:     	add	x9, x26, #0x1
100debfe4:     	ldsetb	w8, w9, [x9]
100debfe8:     	tst	w9, w8
100debfec:     	b.ne	0x100dec018 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1a58>
100debff0:     	ldr	w8, [x28, #0x20]
100debff4:     	cmn	w8, #0x1
100debff8:     	b.eq	0x100dec018 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1a58>
100debffc:     	add	w9, w8, #0x1
100dec000:     	add	x10, x28, #0x20
100dec004:     	mov	x11, x8
100dec008:     	casl	w11, w9, [x10]
100dec00c:     	cmp	w11, w8
100dec010:     	mov	x8, x11
100dec014:     	b.ne	0x100debff4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1a34>
100dec018:     	tbz	w0, #0x0, 0x100dec110 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1b50>
100dec01c:     	ldr	w8, [x19, #0x4]
100dec020:     	add	x8, x21, x8
100dec024:     	add	x8, x8, #0x1
100dec028:     	lsr	x9, x8, #32
100dec02c:     	cbnz	x9, 0x100df1244 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c84>
100dec030:     	tbz	w21, #0x1f, 0x100dec140 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1b80>
100dec034:     	ldr	x9, [x20, #0x77f0]
100dec038:     	ldapurb	w9, [x9, #0x18]
100dec03c:     	cbnz	w9, 0x100df1334 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6d74>
100dec040:     	str	w8, [x19, #0x4]
100dec044:     	ldr	x2, [sp, #0x328]
100dec048:     	ldr	x8, [sp, #0x340]
100dec04c:     	cbz	x8, 0x100decb64 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x25a4>
100dec050:     	ldr	x10, [sp, #0x338]
100dec054:     	cbz	x10, 0x100dec06c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1aac>
100dec058:     	ldr	x9, [sp, #0x2e0]
100dec05c:     	ldr	x8, [x9, #0x10]
100dec060:     	adds	x8, x8, x10
100dec064:     	csinv	x8, x8, xzr, lo
100dec068:     	str	x8, [x9, #0x10]
100dec06c:     	ldr	x4, [sp, #0x360]
100dec070:     	str	xzr, [sp, #0x338]
100dec074:     	add	x0, sp, #0x420
100dec078:     	mov	x1, x20
100dec07c:     	ldr	x3, [sp, #0x308]
100dec080:     	bl	0x100da521c <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp8jit_callNtB6_11Interpreter27note_backedge_and_maybe_osr>
100dec084:     	ldrb	w9, [sp, #0x420]
100dec088:     	cmp	w9, #0xff
100dec08c:     	b.ne	0x100df12c0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6d00>
100dec090:     	ldr	x8, [sp, #0x428]
100dec094:     	ldr	x19, [sp, #0x308]
100dec098:     	cbnz	x8, 0x100df12f4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6d34>
100dec09c:     	str	xzr, [sp, #0x338]
100dec0a0:     	ldr	x8, [sp, #0x358]
100dec0a4:     	b	0x100dea894 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2d4>
100dec0a8:     	ldrb	w8, [x26, #0x2]
100dec0ac:     	and	w9, w8, #0xff
100dec0b0:     	add	w10, w9, #0x1
100dec0b4:     	cmp	w10, #0xff
100dec0b8:     	mov	w10, #0xff              ; =255
100dec0bc:     	csinc	w10, w10, w9, hs
100dec0c0:     	add	x11, x26, #0x2
100dec0c4:     	casb	w8, w10, [x11]
100dec0c8:     	cmp	w8, w9
100dec0cc:     	b.ne	0x100dec0ac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1aec>
100dec0d0:     	mov	w8, #0x20               ; =32
100dec0d4:     	add	x9, x26, #0x1
100dec0d8:     	ldsetb	w8, w9, [x9]
100dec0dc:     	tst	w9, w8
100dec0e0:     	b.ne	0x100dec10c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1b4c>
100dec0e4:     	ldr	w8, [x28, #0x20]
100dec0e8:     	cmn	w8, #0x1
100dec0ec:     	b.eq	0x100dec10c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1b4c>
100dec0f0:     	add	w9, w8, #0x1
100dec0f4:     	add	x10, x28, #0x20
100dec0f8:     	mov	x11, x8
100dec0fc:     	casl	w11, w9, [x10]
100dec100:     	cmp	w11, w8
100dec104:     	mov	x8, x11
100dec108:     	b.ne	0x100dec0e8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1b28>
100dec10c:     	tbz	w0, #0x0, 0x100dec128 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1b68>
100dec110:     	ldr	w8, [x19, #0x4]
100dec114:     	add	w8, w8, #0x1
100dec118:     	str	w8, [x19, #0x4]
100dec11c:     	ldr	x19, [sp, #0x308]
100dec120:     	ldr	x8, [sp, #0x358]
100dec124:     	b	0x100dea894 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2d4>
100dec128:     	ldr	w8, [x19, #0x4]
100dec12c:     	add	x8, x21, x8
100dec130:     	add	x8, x8, #0x1
100dec134:     	lsr	x9, x8, #32
100dec138:     	cbnz	x9, 0x100df1244 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c84>
100dec13c:     	tbnz	w21, #0x1f, 0x100decaa4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x24e4>
100dec140:     	str	w8, [x19, #0x4]
100dec144:     	ldr	x19, [sp, #0x308]
100dec148:     	ldr	x8, [sp, #0x358]
100dec14c:     	b	0x100dea894 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2d4>
100dec150:     	fmov	d0, x9
100dec154:     	frintz	d0, d0
100dec158:     	mov	x9, #0x3df0000000000000 ; =4463067230724161536
100dec15c:     	fmov	d1, x9
100dec160:     	fmul	d1, d0, d1
100dec164:     	frintz	d1, d1
100dec168:     	mov	x9, #-0x3e10000000000000 ; =-4472074429978902528
100dec16c:     	fmov	d2, x9
100dec170:     	fmadd	d1, d1, d2, d0
100dec174:     	movi.2d	v3, #0xffffffffffffffff
100dec178:     	fneg.2d	v3, v3
100dec17c:     	bit.16b	v0, v1, v3
100dec180:     	mov	x9, #0x41f0000000000000 ; =4751297606875873280
100dec184:     	fmov	d1, x9
100dec188:     	fadd	d1, d0, d1
100dec18c:     	fcmp	d0, #0.0
100dec190:     	fcsel	d0, d1, d0, mi
100dec194:     	fadd	d1, d0, d2
100dec198:     	mov	x9, #0x41e0000000000000 ; =4746794007248502784
100dec19c:     	fmov	d2, x9
100dec1a0:     	fcmp	d0, d2
100dec1a4:     	fcsel	d0, d0, d1, lt
100dec1a8:     	fcvtzs	w9, d0
100dec1ac:     	cbz	x28, 0x100dec1f4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1c34>
100dec1b0:     	mov	x10, #-0x2000000000001  ; =-562949953421313
100dec1b4:     	cmp	x8, x10
100dec1b8:     	mov	w8, #0x3                ; =3
100dec1bc:     	csinc	w8, w8, wzr, ls
100dec1c0:     	ldsetb	w8, w10, [x26]
100dec1c4:     	bics	wzr, w8, w10
100dec1c8:     	b.eq	0x100dec1f4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1c34>
100dec1cc:     	ldr	w8, [x28, #0x20]
100dec1d0:     	cmn	w8, #0x1
100dec1d4:     	b.eq	0x100dec1f4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1c34>
100dec1d8:     	add	w10, w8, #0x1
100dec1dc:     	add	x11, x28, #0x20
100dec1e0:     	mov	x12, x8
100dec1e4:     	casl	w12, w10, [x11]
100dec1e8:     	cmp	w12, w8
100dec1ec:     	mov	x8, x12
100dec1f0:     	b.ne	0x100dec1d0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1c10>
100dec1f4:     	and	w8, w9, w7
100dec1f8:     	orr	x8, x8, #0xfffe000000000000
100dec1fc:     	b	0x100decca0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x26e0>
100dec200:     	frintz	d1, d1
100dec204:     	mov	x10, #0x3df0000000000000 ; =4463067230724161536
100dec208:     	fmov	d2, x10
100dec20c:     	fmul	d2, d1, d2
100dec210:     	frintz	d2, d2
100dec214:     	mov	x10, #-0x3e10000000000000 ; =-4472074429978902528
100dec218:     	fmov	d3, x10
100dec21c:     	fmadd	d2, d2, d3, d1
100dec220:     	movi.2d	v4, #0xffffffffffffffff
100dec224:     	fneg.2d	v4, v4
100dec228:     	bit.16b	v1, v2, v4
100dec22c:     	mov	x10, #0x41f0000000000000 ; =4751297606875873280
100dec230:     	fmov	d2, x10
100dec234:     	fadd	d2, d1, d2
100dec238:     	fcmp	d1, #0.0
100dec23c:     	fcsel	d1, d2, d1, mi
100dec240:     	fadd	d2, d1, d3
100dec244:     	mov	x10, #0x41e0000000000000 ; =4746794007248502784
100dec248:     	fmov	d3, x10
100dec24c:     	fcmp	d1, d3
100dec250:     	fcsel	d1, d1, d2, lt
100dec254:     	fcvtzs	w10, d1
100dec258:     	tbnz	w12, #0x0, 0x100deb7ac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x11ec>
100dec25c:     	cbz	x28, 0x100dec340 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1d80>
100dec260:     	mov	x12, #-0x2000000000001  ; =-562949953421313
100dec264:     	cmp	x9, x12
100dec268:     	mov	w9, #0x1                ; =1
100dec26c:     	cinc	w13, w9, ls
100dec270:     	cmp	x8, x12
100dec274:     	cinc	w8, w9, ls
100dec278:     	orr	w8, w8, w13
100dec27c:     	ldsetb	w8, w12, [x26]
100dec280:     	bics	wzr, w8, w12
100dec284:     	b.eq	0x100dec2b0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1cf0>
100dec288:     	ldr	w8, [x28, #0x20]
100dec28c:     	cmn	w8, #0x1
100dec290:     	b.eq	0x100dec2b0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1cf0>
100dec294:     	add	w12, w8, #0x1
100dec298:     	add	x13, x28, #0x20
100dec29c:     	mov	x14, x8
100dec2a0:     	casl	w14, w12, [x13]
100dec2a4:     	cmp	w14, w8
100dec2a8:     	mov	x8, x14
100dec2ac:     	b.ne	0x100dec28c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1ccc>
100dec2b0:     	ldsetb	w9, w8, [x26]
100dec2b4:     	tbnz	w8, #0x0, 0x100dec340 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1d80>
100dec2b8:     	ldr	w8, [x28, #0x20]
100dec2bc:     	cmn	w8, #0x1
100dec2c0:     	b.eq	0x100dec340 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1d80>
100dec2c4:     	add	w9, w8, #0x1
100dec2c8:     	add	x12, x28, #0x20
100dec2cc:     	mov	x13, x8
100dec2d0:     	casl	w13, w9, [x12]
100dec2d4:     	cmp	w13, w8
100dec2d8:     	mov	x8, x13
100dec2dc:     	b.ne	0x100dec2bc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1cfc>
100dec2e0:     	b	0x100dec340 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1d80>
100dec2e4:     	frintz	d0, d0
100dec2e8:     	mov	x11, #0x3df0000000000000 ; =4463067230724161536
100dec2ec:     	fmov	d1, x11
100dec2f0:     	fmul	d1, d0, d1
100dec2f4:     	frintz	d1, d1
100dec2f8:     	mov	x11, #-0x3e10000000000000 ; =-4472074429978902528
100dec2fc:     	fmov	d2, x11
100dec300:     	fmadd	d1, d1, d2, d0
100dec304:     	movi.2d	v3, #0xffffffffffffffff
100dec308:     	fneg.2d	v3, v3
100dec30c:     	bit.16b	v0, v1, v3
100dec310:     	mov	x11, #0x41f0000000000000 ; =4751297606875873280
100dec314:     	fmov	d1, x11
100dec318:     	fadd	d1, d0, d1
100dec31c:     	fcmp	d0, #0.0
100dec320:     	fcsel	d0, d1, d0, mi
100dec324:     	fadd	d1, d0, d2
100dec328:     	mov	x11, #0x41e0000000000000 ; =4746794007248502784
100dec32c:     	fmov	d2, x11
100dec330:     	fcmp	d0, d2
100dec334:     	fcsel	d0, d0, d1, lt
100dec338:     	fcvtzs	w11, d0
100dec33c:     	cbnz	x28, 0x100dec260 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1ca0>
100dec340:     	orr	w8, w11, w10
100dec344:     	orr	x8, x8, #0xfffe000000000000
100dec348:     	b	0x100decca0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x26e0>
100dec34c:     	frintz	d1, d1
100dec350:     	mov	x10, #0x3df0000000000000 ; =4463067230724161536
100dec354:     	fmov	d2, x10
100dec358:     	fmul	d2, d1, d2
100dec35c:     	frintz	d2, d2
100dec360:     	mov	x10, #-0x3e10000000000000 ; =-4472074429978902528
100dec364:     	fmov	d3, x10
100dec368:     	fmadd	d2, d2, d3, d1
100dec36c:     	movi.2d	v4, #0xffffffffffffffff
100dec370:     	fneg.2d	v4, v4
100dec374:     	bit.16b	v1, v2, v4
100dec378:     	mov	x10, #0x41f0000000000000 ; =4751297606875873280
100dec37c:     	fmov	d2, x10
100dec380:     	fadd	d2, d1, d2
100dec384:     	fcmp	d1, #0.0
100dec388:     	fcsel	d1, d2, d1, mi
100dec38c:     	fadd	d2, d1, d3
100dec390:     	mov	x10, #0x41e0000000000000 ; =4746794007248502784
100dec394:     	fmov	d3, x10
100dec398:     	fcmp	d1, d3
100dec39c:     	fcsel	d1, d1, d2, lt
100dec3a0:     	fcvtzs	w10, d1
100dec3a4:     	tbnz	w12, #0x0, 0x100deb7f4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1234>
100dec3a8:     	cbz	x28, 0x100dec48c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1ecc>
100dec3ac:     	mov	x12, #-0x2000000000001  ; =-562949953421313
100dec3b0:     	cmp	x9, x12
100dec3b4:     	mov	w9, #0x1                ; =1
100dec3b8:     	cinc	w13, w9, ls
100dec3bc:     	cmp	x8, x12
100dec3c0:     	cinc	w8, w9, ls
100dec3c4:     	orr	w8, w8, w13
100dec3c8:     	ldsetb	w8, w12, [x26]
100dec3cc:     	bics	wzr, w8, w12
100dec3d0:     	b.eq	0x100dec3fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1e3c>
100dec3d4:     	ldr	w8, [x28, #0x20]
100dec3d8:     	cmn	w8, #0x1
100dec3dc:     	b.eq	0x100dec3fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1e3c>
100dec3e0:     	add	w12, w8, #0x1
100dec3e4:     	add	x13, x28, #0x20
100dec3e8:     	mov	x14, x8
100dec3ec:     	casl	w14, w12, [x13]
100dec3f0:     	cmp	w14, w8
100dec3f4:     	mov	x8, x14
100dec3f8:     	b.ne	0x100dec3d8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1e18>
100dec3fc:     	ldsetb	w9, w8, [x26]
100dec400:     	tbnz	w8, #0x0, 0x100dec48c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1ecc>
100dec404:     	ldr	w8, [x28, #0x20]
100dec408:     	cmn	w8, #0x1
100dec40c:     	b.eq	0x100dec48c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1ecc>
100dec410:     	add	w9, w8, #0x1
100dec414:     	add	x12, x28, #0x20
100dec418:     	mov	x13, x8
100dec41c:     	casl	w13, w9, [x12]
100dec420:     	cmp	w13, w8
100dec424:     	mov	x8, x13
100dec428:     	b.ne	0x100dec408 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1e48>
100dec42c:     	b	0x100dec48c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1ecc>
100dec430:     	frintz	d0, d0
100dec434:     	mov	x11, #0x3df0000000000000 ; =4463067230724161536
100dec438:     	fmov	d1, x11
100dec43c:     	fmul	d1, d0, d1
100dec440:     	frintz	d1, d1
100dec444:     	mov	x11, #-0x3e10000000000000 ; =-4472074429978902528
100dec448:     	fmov	d2, x11
100dec44c:     	fmadd	d1, d1, d2, d0
100dec450:     	movi.2d	v3, #0xffffffffffffffff
100dec454:     	fneg.2d	v3, v3
100dec458:     	bit.16b	v0, v1, v3
100dec45c:     	mov	x11, #0x41f0000000000000 ; =4751297606875873280
100dec460:     	fmov	d1, x11
100dec464:     	fadd	d1, d0, d1
100dec468:     	fcmp	d0, #0.0
100dec46c:     	fcsel	d0, d1, d0, mi
100dec470:     	fadd	d1, d0, d2
100dec474:     	mov	x11, #0x41e0000000000000 ; =4746794007248502784
100dec478:     	fmov	d2, x11
100dec47c:     	fcmp	d0, d2
100dec480:     	fcsel	d0, d0, d1, lt
100dec484:     	fcvtzs	w11, d0
100dec488:     	cbnz	x28, 0x100dec3ac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1dec>
100dec48c:     	eor	w8, w11, w10
100dec490:     	orr	x8, x8, #0xfffe000000000000
100dec494:     	b	0x100decca0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x26e0>
100dec498:     	adds	w8, w10, w9
100dec49c:     	b.vs	0x100ded224 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c64>
100dec4a0:     	orr	x8, x8, #0xfffe000000000000
100dec4a4:     	cbz	x28, 0x100decca0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x26e0>
100dec4a8:     	mov	x11, #-0x2000000000001  ; =-562949953421313
100dec4ac:     	cmp	x10, x11
100dec4b0:     	mov	w10, #0x1               ; =1
100dec4b4:     	cinc	w12, w10, ls
100dec4b8:     	cmp	x9, x11
100dec4bc:     	cinc	w9, w10, ls
100dec4c0:     	orr	w9, w9, w12
100dec4c4:     	ldsetb	w9, w10, [x26]
100dec4c8:     	bics	wzr, w9, w10
100dec4cc:     	b.eq	0x100dec4f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1f38>
100dec4d0:     	ldr	w9, [x28, #0x20]
100dec4d4:     	cmn	w9, #0x1
100dec4d8:     	b.eq	0x100dec4f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1f38>
100dec4dc:     	add	w10, w9, #0x1
100dec4e0:     	add	x11, x28, #0x20
100dec4e4:     	mov	x12, x9
100dec4e8:     	casl	w12, w10, [x11]
100dec4ec:     	cmp	w12, w9
100dec4f0:     	mov	x9, x12
100dec4f4:     	b.ne	0x100dec4d4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1f14>
100dec4f8:     	and	x9, x8, #0xfffe000000000000
100dec4fc:     	mov	x10, #-0x2000000000000  ; =-562949953421312
100dec500:     	cmp	x9, x10
100dec504:     	b.eq	0x100dec9bc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x23fc>
100dec508:     	cbnz	x9, 0x100dec9c8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2408>
100dec50c:     	mov	x10, #0x2               ; =2
100dec510:     	movk	x10, #0xfffe, lsl #48
100dec514:     	mov	w9, #0x10               ; =16
100dec518:     	tst	x8, x10
100dec51c:     	b.ne	0x100decc18 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2658>
100dec520:     	mov	w10, #-0x3              ; =-3
100dec524:     	mov	w11, #0x10              ; =16
100dec528:     	and	x10, x8, x10
100dec52c:     	cbz	x10, 0x100decc68 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x26a8>
100dec530:     	adrp	x11, 0x1034f4000 <_global_added_lock+0x68>
100dec534:     	add	x11, x11, #0xef8
100dec538:     	ldr	x9, [x11]
100dec53c:     	ldrb	w9, [x9, x10]
100dec540:     	cmp	w9, #0x20
100dec544:     	b.ne	0x100decc20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2660>
100dec548:     	mov	w9, #0x4                ; =4
100dec54c:     	b	0x100decc38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2678>
100dec550:     	fmov	d1, x11
100dec554:     	frintz	d1, d1
100dec558:     	mov	x11, #0x3df0000000000000 ; =4463067230724161536
100dec55c:     	fmov	d2, x11
100dec560:     	fmul	d2, d1, d2
100dec564:     	frintz	d2, d2
100dec568:     	mov	x11, #-0x3e10000000000000 ; =-4472074429978902528
100dec56c:     	fmov	d3, x11
100dec570:     	fmadd	d2, d2, d3, d1
100dec574:     	movi.2d	v4, #0xffffffffffffffff
100dec578:     	fneg.2d	v4, v4
100dec57c:     	bit.16b	v1, v2, v4
100dec580:     	mov	x11, #0x41f0000000000000 ; =4751297606875873280
100dec584:     	fmov	d2, x11
100dec588:     	fadd	d2, d1, d2
100dec58c:     	fcmp	d1, #0.0
100dec590:     	fcsel	d1, d2, d1, mi
100dec594:     	fadd	d2, d1, d3
100dec598:     	mov	x11, #0x41e0000000000000 ; =4746794007248502784
100dec59c:     	fmov	d3, x11
100dec5a0:     	fcmp	d1, d3
100dec5a4:     	fcsel	d1, d1, d2, lt
100dec5a8:     	fcvtzs	w11, d1
100dec5ac:     	cbz	w12, 0x100dec620 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2060>
100dec5b0:     	fmov	x10, d0
100dec5b4:     	ubfx	x10, x10, #52, #11
100dec5b8:     	cmp	x10, #0x7fe
100dec5bc:     	b.ls	0x100dec5c8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2008>
100dec5c0:     	mov	w10, #0x0               ; =0
100dec5c4:     	b	0x100dec620 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2060>
100dec5c8:     	frintz	d0, d0
100dec5cc:     	mov	x10, #0x3df0000000000000 ; =4463067230724161536
100dec5d0:     	fmov	d1, x10
100dec5d4:     	fmul	d1, d0, d1
100dec5d8:     	frintz	d1, d1
100dec5dc:     	mov	x10, #-0x3e10000000000000 ; =-4472074429978902528
100dec5e0:     	fmov	d2, x10
100dec5e4:     	fmadd	d1, d1, d2, d0
100dec5e8:     	movi.2d	v3, #0xffffffffffffffff
100dec5ec:     	fneg.2d	v3, v3
100dec5f0:     	bit.16b	v0, v1, v3
100dec5f4:     	mov	x10, #0x41f0000000000000 ; =4751297606875873280
100dec5f8:     	fmov	d1, x10
100dec5fc:     	fadd	d1, d0, d1
100dec600:     	fcmp	d0, #0.0
100dec604:     	fcsel	d0, d1, d0, mi
100dec608:     	fadd	d1, d0, d2
100dec60c:     	mov	x10, #0x41e0000000000000 ; =4746794007248502784
100dec610:     	fmov	d2, x10
100dec614:     	fcmp	d0, d2
100dec618:     	fcsel	d0, d0, d1, lt
100dec61c:     	fcvtzs	w10, d0
100dec620:     	cbz	x28, 0x100dec6a4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x20e4>
100dec624:     	mov	x12, #-0x2000000000001  ; =-562949953421313
100dec628:     	cmp	x9, x12
100dec62c:     	mov	w9, #0x1                ; =1
100dec630:     	cinc	w13, w9, ls
100dec634:     	cmp	x8, x12
100dec638:     	cinc	w8, w9, ls
100dec63c:     	orr	w8, w8, w13
100dec640:     	ldsetb	w8, w12, [x26]
100dec644:     	bics	wzr, w8, w12
100dec648:     	b.eq	0x100dec674 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x20b4>
100dec64c:     	ldr	w8, [x28, #0x20]
100dec650:     	cmn	w8, #0x1
100dec654:     	b.eq	0x100dec674 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x20b4>
100dec658:     	add	w12, w8, #0x1
100dec65c:     	add	x13, x28, #0x20
100dec660:     	mov	x14, x8
100dec664:     	casl	w14, w12, [x13]
100dec668:     	cmp	w14, w8
100dec66c:     	mov	x8, x14
100dec670:     	b.ne	0x100dec650 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2090>
100dec674:     	ldsetb	w9, w8, [x26]
100dec678:     	tbnz	w8, #0x0, 0x100dec6a4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x20e4>
100dec67c:     	ldr	w8, [x28, #0x20]
100dec680:     	cmn	w8, #0x1
100dec684:     	b.eq	0x100dec6a4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x20e4>
100dec688:     	add	w9, w8, #0x1
100dec68c:     	add	x12, x28, #0x20
100dec690:     	mov	x13, x8
100dec694:     	casl	w13, w9, [x12]
100dec698:     	cmp	w13, w8
100dec69c:     	mov	x8, x13
100dec6a0:     	b.ne	0x100dec680 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x20c0>
100dec6a4:     	asr	w8, w10, w11
100dec6a8:     	orr	x8, x8, #0xfffe000000000000
100dec6ac:     	b	0x100decca0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x26e0>
100dec6b0:     	frintz	d1, d1
100dec6b4:     	mov	x10, #0x3df0000000000000 ; =4463067230724161536
100dec6b8:     	fmov	d2, x10
100dec6bc:     	fmul	d2, d1, d2
100dec6c0:     	frintz	d2, d2
100dec6c4:     	mov	x10, #-0x3e10000000000000 ; =-4472074429978902528
100dec6c8:     	fmov	d3, x10
100dec6cc:     	fmadd	d2, d2, d3, d1
100dec6d0:     	movi.2d	v4, #0xffffffffffffffff
100dec6d4:     	fneg.2d	v4, v4
100dec6d8:     	bit.16b	v1, v2, v4
100dec6dc:     	mov	x10, #0x41f0000000000000 ; =4751297606875873280
100dec6e0:     	fmov	d2, x10
100dec6e4:     	fadd	d2, d1, d2
100dec6e8:     	fcmp	d1, #0.0
100dec6ec:     	fcsel	d1, d2, d1, mi
100dec6f0:     	fadd	d2, d1, d3
100dec6f4:     	mov	x10, #0x41e0000000000000 ; =4746794007248502784
100dec6f8:     	fmov	d3, x10
100dec6fc:     	fcmp	d1, d3
100dec700:     	fcsel	d1, d1, d2, lt
100dec704:     	fcvtzs	w10, d1
100dec708:     	tbnz	w12, #0x0, 0x100deb870 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x12b0>
100dec70c:     	cbz	x28, 0x100dec7f0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2230>
100dec710:     	mov	x12, #-0x2000000000001  ; =-562949953421313
100dec714:     	cmp	x9, x12
100dec718:     	mov	w9, #0x1                ; =1
100dec71c:     	cinc	w13, w9, ls
100dec720:     	cmp	x8, x12
100dec724:     	cinc	w8, w9, ls
100dec728:     	orr	w8, w8, w13
100dec72c:     	ldsetb	w8, w12, [x26]
100dec730:     	bics	wzr, w8, w12
100dec734:     	b.eq	0x100dec760 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x21a0>
100dec738:     	ldr	w8, [x28, #0x20]
100dec73c:     	cmn	w8, #0x1
100dec740:     	b.eq	0x100dec760 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x21a0>
100dec744:     	add	w12, w8, #0x1
100dec748:     	add	x13, x28, #0x20
100dec74c:     	mov	x14, x8
100dec750:     	casl	w14, w12, [x13]
100dec754:     	cmp	w14, w8
100dec758:     	mov	x8, x14
100dec75c:     	b.ne	0x100dec73c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x217c>
100dec760:     	ldsetb	w9, w8, [x26]
100dec764:     	tbnz	w8, #0x0, 0x100dec7f0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2230>
100dec768:     	ldr	w8, [x28, #0x20]
100dec76c:     	cmn	w8, #0x1
100dec770:     	b.eq	0x100dec7f0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2230>
100dec774:     	add	w9, w8, #0x1
100dec778:     	add	x12, x28, #0x20
100dec77c:     	mov	x13, x8
100dec780:     	casl	w13, w9, [x12]
100dec784:     	cmp	w13, w8
100dec788:     	mov	x8, x13
100dec78c:     	b.ne	0x100dec76c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x21ac>
100dec790:     	b	0x100dec7f0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2230>
100dec794:     	frintz	d0, d0
100dec798:     	mov	x11, #0x3df0000000000000 ; =4463067230724161536
100dec79c:     	fmov	d1, x11
100dec7a0:     	fmul	d1, d0, d1
100dec7a4:     	frintz	d1, d1
100dec7a8:     	mov	x11, #-0x3e10000000000000 ; =-4472074429978902528
100dec7ac:     	fmov	d2, x11
100dec7b0:     	fmadd	d1, d1, d2, d0
100dec7b4:     	movi.2d	v3, #0xffffffffffffffff
100dec7b8:     	fneg.2d	v3, v3
100dec7bc:     	bit.16b	v0, v1, v3
100dec7c0:     	mov	x11, #0x41f0000000000000 ; =4751297606875873280
100dec7c4:     	fmov	d1, x11
100dec7c8:     	fadd	d1, d0, d1
100dec7cc:     	fcmp	d0, #0.0
100dec7d0:     	fcsel	d0, d1, d0, mi
100dec7d4:     	fadd	d1, d0, d2
100dec7d8:     	mov	x11, #0x41e0000000000000 ; =4746794007248502784
100dec7dc:     	fmov	d2, x11
100dec7e0:     	fcmp	d0, d2
100dec7e4:     	fcsel	d0, d0, d1, lt
100dec7e8:     	fcvtzs	w11, d0
100dec7ec:     	cbnz	x28, 0x100dec710 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2150>
100dec7f0:     	and	w8, w11, w10
100dec7f4:     	orr	x8, x8, #0xfffe000000000000
100dec7f8:     	b	0x100decca0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x26e0>
100dec7fc:     	fmov	d1, x11
100dec800:     	frintz	d1, d1
100dec804:     	mov	x11, #0x3df0000000000000 ; =4463067230724161536
100dec808:     	fmov	d2, x11
100dec80c:     	fmul	d2, d1, d2
100dec810:     	frintz	d2, d2
100dec814:     	mov	x11, #-0x3e10000000000000 ; =-4472074429978902528
100dec818:     	fmov	d3, x11
100dec81c:     	fmadd	d2, d2, d3, d1
100dec820:     	movi.2d	v4, #0xffffffffffffffff
100dec824:     	fneg.2d	v4, v4
100dec828:     	bit.16b	v1, v2, v4
100dec82c:     	mov	x11, #0x41f0000000000000 ; =4751297606875873280
100dec830:     	fmov	d2, x11
100dec834:     	fadd	d2, d1, d2
100dec838:     	fcmp	d1, #0.0
100dec83c:     	fcsel	d1, d2, d1, mi
100dec840:     	fadd	d2, d1, d3
100dec844:     	mov	x11, #0x41e0000000000000 ; =4746794007248502784
100dec848:     	fmov	d3, x11
100dec84c:     	fcmp	d1, d3
100dec850:     	fcsel	d1, d1, d2, lt
100dec854:     	fcvtzs	w11, d1
100dec858:     	cbz	w12, 0x100dec8cc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x230c>
100dec85c:     	fmov	x10, d0
100dec860:     	ubfx	x10, x10, #52, #11
100dec864:     	cmp	x10, #0x7fe
100dec868:     	b.ls	0x100dec874 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x22b4>
100dec86c:     	mov	w10, #0x0               ; =0
100dec870:     	b	0x100dec8cc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x230c>
100dec874:     	frintz	d0, d0
100dec878:     	mov	x10, #0x3df0000000000000 ; =4463067230724161536
100dec87c:     	fmov	d1, x10
100dec880:     	fmul	d1, d0, d1
100dec884:     	frintz	d1, d1
100dec888:     	mov	x10, #-0x3e10000000000000 ; =-4472074429978902528
100dec88c:     	fmov	d2, x10
100dec890:     	fmadd	d1, d1, d2, d0
100dec894:     	movi.2d	v3, #0xffffffffffffffff
100dec898:     	fneg.2d	v3, v3
100dec89c:     	bit.16b	v0, v1, v3
100dec8a0:     	mov	x10, #0x41f0000000000000 ; =4751297606875873280
100dec8a4:     	fmov	d1, x10
100dec8a8:     	fadd	d1, d0, d1
100dec8ac:     	fcmp	d0, #0.0
100dec8b0:     	fcsel	d0, d1, d0, mi
100dec8b4:     	fadd	d1, d0, d2
100dec8b8:     	mov	x10, #0x41e0000000000000 ; =4746794007248502784
100dec8bc:     	fmov	d2, x10
100dec8c0:     	fcmp	d0, d2
100dec8c4:     	fcsel	d0, d0, d1, lt
100dec8c8:     	fcvtzs	w10, d0
100dec8cc:     	cbz	x28, 0x100dec950 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2390>
100dec8d0:     	mov	x12, #-0x2000000000001  ; =-562949953421313
100dec8d4:     	cmp	x9, x12
100dec8d8:     	mov	w9, #0x1                ; =1
100dec8dc:     	cinc	w13, w9, ls
100dec8e0:     	cmp	x8, x12
100dec8e4:     	cinc	w8, w9, ls
100dec8e8:     	orr	w8, w8, w13
100dec8ec:     	ldsetb	w8, w12, [x26]
100dec8f0:     	bics	wzr, w8, w12
100dec8f4:     	b.eq	0x100dec920 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2360>
100dec8f8:     	ldr	w8, [x28, #0x20]
100dec8fc:     	cmn	w8, #0x1
100dec900:     	b.eq	0x100dec920 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2360>
100dec904:     	add	w12, w8, #0x1
100dec908:     	add	x13, x28, #0x20
100dec90c:     	mov	x14, x8
100dec910:     	casl	w14, w12, [x13]
100dec914:     	cmp	w14, w8
100dec918:     	mov	x8, x14
100dec91c:     	b.ne	0x100dec8fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x233c>
100dec920:     	ldsetb	w9, w8, [x26]
100dec924:     	tbnz	w8, #0x0, 0x100dec950 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2390>
100dec928:     	ldr	w8, [x28, #0x20]
100dec92c:     	cmn	w8, #0x1
100dec930:     	b.eq	0x100dec950 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2390>
100dec934:     	add	w9, w8, #0x1
100dec938:     	add	x12, x28, #0x20
100dec93c:     	mov	x13, x8
100dec940:     	casl	w13, w9, [x12]
100dec944:     	cmp	w13, w8
100dec948:     	mov	x8, x13
100dec94c:     	b.ne	0x100dec92c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x236c>
100dec950:     	lsl	w8, w10, w11
100dec954:     	orr	x8, x8, #0xfffe000000000000
100dec958:     	b	0x100decca0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x26e0>
100dec95c:     	subs	w8, w21, w27
100dec960:     	b.vc	0x100deb3b0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xdf0>
100dec964:     	b	0x100deb3c0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xe00>
100dec968:     	smull	x10, w21, w27
100dec96c:     	cmp	x10, w10, sxtw
100dec970:     	b.ne	0x100deac0c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x64c>
100dec974:     	eor	w8, w27, w21
100dec978:     	tbz	w8, #0x1f, 0x100decb20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2560>
100dec97c:     	cbnz	w10, 0x100decb20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2560>
100dec980:     	mov	x8, #-0x8000000000000000 ; =-9223372036854775808
100dec984:     	str	x8, [sp, #0x428]
100dec988:     	mov	w8, #0x1                ; =1
100dec98c:     	b	0x100deb3a0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xde0>
100dec990:     	mov	w8, #-0x80000000        ; =-2147483648
100dec994:     	cmp	w21, w8
100dec998:     	ccmn	w27, #0x1, #0x0, eq
100dec99c:     	b.eq	0x100deb380 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xdc0>
100dec9a0:     	sdiv	w8, w21, w27
100dec9a4:     	msub	w9, w8, w27, w21
100dec9a8:     	tbz	w21, #0x1f, 0x100decb2c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x256c>
100dec9ac:     	cbnz	w9, 0x100decb2c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x256c>
100dec9b0:     	mov	x8, #-0x8000000000000000 ; =-9223372036854775808
100dec9b4:     	str	x8, [sp, #0x428]
100dec9b8:     	b	0x100deb398 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xdd8>
100dec9bc:     	mov	w9, #0x1                ; =1
100dec9c0:     	mov	w11, #0x1               ; =1
100dec9c4:     	b	0x100decc68 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x26a8>
100dec9c8:     	mov	w9, #0x2                ; =2
100dec9cc:     	mov	w11, #0x2               ; =2
100dec9d0:     	b	0x100decc68 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x26a8>
100dec9d4:     	fmov	x10, d0
100dec9d8:     	mov	x8, #0x2000000000000    ; =562949953421312
100dec9dc:     	add	x8, x10, x8
100dec9e0:     	fcmp	d0, d0
100dec9e4:     	mov	x10, #0x7ffa000000000000 ; =9221683186994511872
100dec9e8:     	csel	x8, x10, x8, vs
100dec9ec:     	cbz	x28, 0x100decc04 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2644>
100dec9f0:     	mov	x10, #-0x2000000000001  ; =-562949953421313
100dec9f4:     	cmp	x9, x10
100dec9f8:     	mov	w9, #0x3                ; =3
100dec9fc:     	csinc	w9, w9, wzr, ls
100deca00:     	ldsetb	w9, w10, [x26]
100deca04:     	bics	wzr, w9, w10
100deca08:     	b.eq	0x100deca34 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2474>
100deca0c:     	ldr	w9, [x28, #0x20]
100deca10:     	cmn	w9, #0x1
100deca14:     	b.eq	0x100deca34 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2474>
100deca18:     	add	w10, w9, #0x1
100deca1c:     	add	x11, x28, #0x20
100deca20:     	mov	x12, x9
100deca24:     	casl	w12, w10, [x11]
100deca28:     	cmp	w12, w9
100deca2c:     	mov	x9, x12
100deca30:     	b.ne	0x100deca10 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2450>
100deca34:     	and	x9, x8, #0xfffe000000000000
100deca38:     	mov	x10, #-0x2000000000000  ; =-562949953421312
100deca3c:     	cmp	x9, x10
100deca40:     	b.eq	0x100deca8c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x24cc>
100deca44:     	cbnz	x9, 0x100deca98 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x24d8>
100deca48:     	mov	x10, #0x2               ; =2
100deca4c:     	movk	x10, #0xfffe, lsl #48
100deca50:     	mov	w9, #0x10               ; =16
100deca54:     	tst	x8, x10
100deca58:     	b.ne	0x100decb7c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x25bc>
100deca5c:     	mov	w10, #-0x3              ; =-3
100deca60:     	mov	w11, #0x10              ; =16
100deca64:     	and	x10, x8, x10
100deca68:     	cbz	x10, 0x100decbcc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x260c>
100deca6c:     	adrp	x11, 0x1034f4000 <_global_added_lock+0x68>
100deca70:     	add	x11, x11, #0xef8
100deca74:     	ldr	x9, [x11]
100deca78:     	ldrb	w9, [x9, x10]
100deca7c:     	cmp	w9, #0x20
100deca80:     	b.ne	0x100decb84 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x25c4>
100deca84:     	mov	w9, #0x4                ; =4
100deca88:     	b	0x100decb9c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x25dc>
100deca8c:     	mov	w9, #0x1                ; =1
100deca90:     	mov	w11, #0x1               ; =1
100deca94:     	b	0x100decbcc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x260c>
100deca98:     	mov	w9, #0x2                ; =2
100deca9c:     	mov	w11, #0x2               ; =2
100decaa0:     	b	0x100decbcc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x260c>
100decaa4:     	ldr	x9, [x20, #0x77f0]
100decaa8:     	ldapurb	w9, [x9, #0x18]
100decaac:     	cbnz	w9, 0x100df1334 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6d74>
100decab0:     	str	w8, [x19, #0x4]
100decab4:     	ldr	x2, [sp, #0x328]
100decab8:     	ldr	x8, [sp, #0x340]
100decabc:     	cbz	x8, 0x100decb70 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x25b0>
100decac0:     	ldr	x10, [sp, #0x338]
100decac4:     	ldr	x3, [sp, #0x308]
100decac8:     	cbz	x10, 0x100decae0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2520>
100decacc:     	ldr	x9, [sp, #0x2e0]
100decad0:     	ldr	x8, [x9, #0x10]
100decad4:     	adds	x8, x8, x10
100decad8:     	csinv	x8, x8, xzr, lo
100decadc:     	str	x8, [x9, #0x10]
100decae0:     	ldr	x4, [sp, #0x360]
100decae4:     	str	xzr, [sp, #0x338]
100decae8:     	add	x0, sp, #0x420
100decaec:     	mov	x1, x20
100decaf0:     	bl	0x100da521c <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp8jit_callNtB6_11Interpreter27note_backedge_and_maybe_osr>
100decaf4:     	ldrb	w9, [sp, #0x420]
100decaf8:     	cmp	w9, #0xff
100decafc:     	b.ne	0x100df12c0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6d00>
100decb00:     	ldr	x8, [sp, #0x428]
100decb04:     	ldr	x19, [sp, #0x308]
100decb08:     	cbnz	x8, 0x100df12f4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6d34>
100decb0c:     	str	xzr, [sp, #0x338]
100decb10:     	ldr	x8, [sp, #0x358]
100decb14:     	b	0x100dea894 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2d4>
100decb18:     	mov	w7, #0x1                ; =1
100decb1c:     	b	0x100deb50c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xf4c>
100decb20:     	mov	w8, #0x0                ; =0
100decb24:     	str	w10, [sp, #0x424]
100decb28:     	b	0x100deb3a0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xde0>
100decb2c:     	mov	w8, #0x0                ; =0
100decb30:     	str	w9, [sp, #0x424]
100decb34:     	str	wzr, [sp, #0x420]
100decb38:     	b	0x100deb3a0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xde0>
100decb3c:     	ldr	w8, [x0, #0x50]
100decb40:     	cbnz	w8, 0x100decec8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2908>
100decb44:     	ldr	x8, [x0, #0x48]
100decb48:     	cbz	x8, 0x100decec0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2900>
100decb4c:     	ldr	w8, [x8, #0x50]
100decb50:     	cbz	w8, 0x100ded248 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c88>
100decb54:     	adds	w8, w8, #0x1
100decb58:     	b.lo	0x100decec4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2904>
100decb5c:     	mov	w8, #0x0                ; =0
100decb60:     	b	0x100decec8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2908>
100decb64:     	ldr	x19, [sp, #0x308]
100decb68:     	ldr	x8, [sp, #0x358]
100decb6c:     	b	0x100dea894 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2d4>
100decb70:     	ldr	x19, [sp, #0x308]
100decb74:     	ldr	x8, [sp, #0x358]
100decb78:     	b	0x100dea894 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2d4>
100decb7c:     	mov	w11, #0x10              ; =16
100decb80:     	b	0x100decbcc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x260c>
100decb84:     	ldr	x9, [x11]
100decb88:     	ldrb	w9, [x9, x10]
100decb8c:     	mov	w12, #0x10              ; =16
100decb90:     	mov	w13, #0x8               ; =8
100decb94:     	cmp	w9, #0x25
100decb98:     	csel	w9, w13, w12, eq
100decb9c:     	ldr	x12, [x11]
100decba0:     	ldrb	w12, [x12, x10]
100decba4:     	cmp	w12, #0x20
100decba8:     	b.ne	0x100decbb4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x25f4>
100decbac:     	mov	w11, #0x4               ; =4
100decbb0:     	b	0x100decbcc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x260c>
100decbb4:     	ldr	x11, [x11]
100decbb8:     	ldrb	w10, [x11, x10]
100decbbc:     	mov	w11, #0x10              ; =16
100decbc0:     	mov	w12, #0x8               ; =8
100decbc4:     	cmp	w10, #0x25
100decbc8:     	csel	w11, w12, w11, eq
100decbcc:     	orr	w9, w11, w9
100decbd0:     	ldsetb	w9, w10, [x26]
100decbd4:     	bics	wzr, w9, w10
100decbd8:     	b.eq	0x100decc04 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2644>
100decbdc:     	ldr	w9, [x28, #0x20]
100decbe0:     	cmn	w9, #0x1
100decbe4:     	b.eq	0x100decc04 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2644>
100decbe8:     	add	w10, w9, #0x1
100decbec:     	add	x11, x28, #0x20
100decbf0:     	mov	x12, x9
100decbf4:     	casl	w12, w10, [x11]
100decbf8:     	cmp	w12, w9
100decbfc:     	mov	x9, x12
100decc00:     	b.ne	0x100decbe0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2620>
100decc04:     	ldr	x9, [sp, #0x358]
100decc08:     	ldr	x10, [x9, #0x10]
100decc0c:     	and	x11, x5, #0xffff
100decc10:     	str	x8, [x10, x11, lsl #3]
100decc14:     	b	0x100deccac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x26ec>
100decc18:     	mov	w11, #0x10              ; =16
100decc1c:     	b	0x100decc68 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x26a8>
100decc20:     	ldr	x9, [x11]
100decc24:     	ldrb	w9, [x9, x10]
100decc28:     	mov	w12, #0x10              ; =16
100decc2c:     	mov	w13, #0x8               ; =8
100decc30:     	cmp	w9, #0x25
100decc34:     	csel	w9, w13, w12, eq
100decc38:     	ldr	x12, [x11]
100decc3c:     	ldrb	w12, [x12, x10]
100decc40:     	cmp	w12, #0x20
100decc44:     	b.ne	0x100decc50 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2690>
100decc48:     	mov	w11, #0x4               ; =4
100decc4c:     	b	0x100decc68 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x26a8>
100decc50:     	ldr	x11, [x11]
100decc54:     	ldrb	w10, [x11, x10]
100decc58:     	mov	w11, #0x10              ; =16
100decc5c:     	mov	w12, #0x8               ; =8
100decc60:     	cmp	w10, #0x25
100decc64:     	csel	w11, w12, w11, eq
100decc68:     	orr	w9, w11, w9
100decc6c:     	ldsetb	w9, w10, [x26]
100decc70:     	bics	wzr, w9, w10
100decc74:     	b.eq	0x100decca0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x26e0>
100decc78:     	ldr	w9, [x28, #0x20]
100decc7c:     	cmn	w9, #0x1
100decc80:     	b.eq	0x100decca0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x26e0>
100decc84:     	add	w10, w9, #0x1
100decc88:     	add	x11, x28, #0x20
100decc8c:     	mov	x12, x9
100decc90:     	casl	w12, w10, [x11]
100decc94:     	cmp	w12, w9
100decc98:     	mov	x9, x12
100decc9c:     	b.ne	0x100decc7c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x26bc>
100decca0:     	ldr	x9, [sp, #0x358]
100decca4:     	ldr	x10, [x9, #0x10]
100decca8:     	str	x8, [x10, x5, lsl #3]
100deccac:     	ldr	w8, [x9, #0x4]
100deccb0:     	add	w8, w8, #0x1
100deccb4:     	str	w8, [x9, #0x4]
100deccb8:     	ldr	x8, [sp, #0x358]
100deccbc:     	b	0x100dea894 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2d4>
100deccc0:     	mov	w24, #0x6               ; =6
100deccc4:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100deccc8:     	add	x2, x2, #0x368
100decccc:     	ldr	x0, [sp, #0x328]
100deccd0:     	mov	x1, x22
100deccd4:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100deccd8:     	mov	x22, #0x0               ; =0
100deccdc:     	ldrh	w8, [x0, #0x8]
100decce0:     	mov	w28, #0x1               ; =1
100decce4:     	ldr	x9, [sp, #0x268]
100decce8:     	cmp	w8, w9, uxth
100deccec:     	b.ls	0x100df1cfc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x773c>
100deccf0:     	ldr	x9, [x0, #0x10]
100deccf4:     	mov	w26, #0x1               ; =1
100deccf8:     	cbz	x9, 0x100df1d00 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7740>
100deccfc:     	ldr	x8, [sp, #0x268]
100decd00:     	str	x19, [x9, x8, lsl #3]
100decd04:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100decd08:     	add	x2, x2, #0x380
100decd0c:     	ldr	x0, [sp, #0x328]
100decd10:     	ldr	x1, [sp, #0x348]
100decd14:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100decd18:     	mov	x22, #0x0               ; =0
100decd1c:     	ldrh	w8, [x0, #0x8]
100decd20:     	mov	w28, #0x1               ; =1
100decd24:     	ldr	w9, [sp, #0x258]
100decd28:     	cmp	w8, w9
100decd2c:     	b.ls	0x100df1cfc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x773c>
100decd30:     	ldr	x9, [x0, #0x10]
100decd34:     	mov	w26, #0x1               ; =1
100decd38:     	cbz	x9, 0x100df1d00 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7740>
100decd3c:     	ldr	x8, [sp, #0x270]
100decd40:     	str	x24, [x9, x8, lsl #3]
100decd44:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100decd48:     	add	x2, x2, #0x398
100decd4c:     	ldr	x0, [sp, #0x328]
100decd50:     	ldr	x1, [sp, #0x348]
100decd54:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100decd58:     	ldr	w8, [x0, #0x4]
100decd5c:     	cmn	w8, #0x1
100decd60:     	b.eq	0x100df1cf4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7734>
100decd64:     	add	w8, w8, #0x1
100decd68:     	str	w8, [x0, #0x4]
100decd6c:     	mov	w22, #0x2               ; =2
100decd70:     	ldr	x19, [sp, #0x308]
100decd74:     	ldr	x8, [sp, #0x358]
100decd78:     	b	0x100dea894 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2d4>
100decd7c:     	ldr	x9, [x12]
100decd80:     	add	x9, x9, x22
100decd84:     	ldr	w10, [x9, #0x8]
100decd88:     	sub	w11, w10, #0x2
100decd8c:     	cmp	w10, #0x1
100decd90:     	csel	w8, w11, w8, hi
100decd94:     	tst	w8, #0xfffffffd
100decd98:     	b.ne	0x100decdcc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x280c>
100decd9c:     	ldr	w0, [x9, #0x18]
100decda0:     	ldr	x1, [x9, #0x10]
100decda4:     	ldr	x8, [x12]
100decda8:     	add	x8, x8, x0
100decdac:     	ldr	x8, [x8, #0x8]
100decdb0:     	cmp	x1, x8
100decdb4:     	b.hs	0x100decdcc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x280c>
100decdb8:     	mov	x19, x12
100decdbc:     	bl	0x100ef313c <__RNvNtCsgTc3P8JF8aG_8otter_vm5array19plain_dense_element>
100decdc0:     	cmp	x0, #0x1
100decdc4:     	mov	x12, x19
100decdc8:     	b.ne	0x100ded000 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2a40>
100decdcc:     	ldr	x8, [x12]
100decdd0:     	add	x8, x8, x22
100decdd4:     	ldr	w9, [x8, #0x8]
100decdd8:     	cmp	w9, #0xb
100decddc:     	ldr	x10, [sp, #0x308]
100decde0:     	b.ne	0x100df0b80 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x65c0>
100decde4:     	ldr	w9, [x8, #0x10]
100decde8:     	ldp	x28, x21, [x8, #0x18]
100decdec:     	stur	x21, [x29, #-0x80]
100decdf0:     	mov	x19, x12
100decdf4:     	tbnz	w9, #0x0, 0x100dece34 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2874>
100decdf8:     	sub	x8, x29, #0x80
100decdfc:     	stur	x8, [x29, #-0x78]
100dece00:     	ldr	x8, [sp, #0x328]
100dece04:     	stp	x8, x10, [x29, #-0x70]
100dece08:     	add	x0, sp, #0x420
100dece0c:     	sub	x2, x29, #0x78
100dece10:     	mov	x1, x20
100dece14:     	bl	0x100c30cf4 <__RINvMs6_NtCsgTc3P8JF8aG_8otter_vm7handlesNtB8_11Interpreter17with_handle_scopeINtNtCsjIpoRzUjyHQ_4core6result6ResultTNtNtB8_5value5ValueB1R_ENtNtNtB8_18runtime_activation16committed_values19CommittedValueErrorENCINvMNtB8_12iterator_opsBD_19drive_iterator_nextNtNtB8_10executable11OperandViewEs6_0EB8_>
100dece18:     	mov	x8, x26
100dece1c:     	ldr	x26, [sp, #0x420]
100dece20:     	ldr	x28, [sp, #0x428]
100dece24:     	ldr	x21, [sp, #0x430]
100dece28:     	cmp	x26, #0x2
100dece2c:     	b.ne	0x100df1c98 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x76d8>
100dece30:     	mov	x26, x8
100dece34:     	mov	x0, x28
100dece38:     	bl	0x100e667ac <__RNvNtCsgTc3P8JF8aG_8otter_vm12abstract_ops11is_callable>
100dece3c:     	tbz	w0, #0x0, 0x100df1bf4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7634>
100dece40:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100dece44:     	add	x2, x2, #0x3f8
100dece48:     	ldr	x0, [sp, #0x328]
100dece4c:     	ldr	x1, [sp, #0x348]
100dece50:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100dece54:     	mov	x1, x0
100dece58:     	mov	x0, x20
100dece5c:     	bl	0x100d979b0 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp6framesNtB6_11Interpreter17frame_ensure_cold>
100dece60:     	mov	w8, #0x1                ; =1
100dece64:     	stp	x8, x23, [x0]
100dece68:     	str	w26, [x0, #0x10]
100dece6c:     	ldr	x5, [sp, #0x268]
100dece70:     	strh	w5, [x0, #0x14]
100dece74:     	ldr	w8, [sp, #0x258]
100dece78:     	strh	w8, [x0, #0x16]
100dece7c:     	strh	w5, [x0, #0x18]
100dece80:     	ldr	x8, [x19]
100dece84:     	add	x8, x8, x22
100dece88:     	ldr	w9, [x8, #0x8]
100dece8c:     	ldr	x8, [x8, #0x20]
100dece90:     	cmp	w9, #0xb
100dece94:     	csel	x3, x8, x21, eq
100dece98:     	str	xzr, [sp, #0x460]
100dece9c:     	sub	x0, x29, #0x78
100decea0:     	add	x4, sp, #0x420
100decea4:     	ldr	x1, [sp, #0x328]
100decea8:     	mov	x2, x28
100deceac:     	bl	0x100de20bc <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter6invoke>
100deceb0:     	ldurb	w8, [x29, #-0x78]
100deceb4:     	cmp	w8, #0xff
100deceb8:     	b.eq	0x100decd6c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x27ac>
100decebc:     	b	0x100df1c08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7648>
100decec0:     	mov	w8, #0x1                ; =1
100decec4:     	str	w8, [x0, #0x50]
100decec8:     	mov	w8, w8
100dececc:     	sub	x28, x8, #0x1
100deced0:     	b	0x100deba28 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1468>
100deced4:     	mov	w21, #0xa               ; =10
100deced8:     	cbz	x23, 0x100decf20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2960>
100decedc:     	mov	x8, #0x2                ; =2
100decee0:     	movk	x8, #0xfffe, lsl #48
100decee4:     	and	x8, x23, x8
100decee8:     	cbnz	x8, 0x100decf20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2960>
100deceec:     	cbz	w23, 0x100decf18 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2958>
100decef0:     	adrp	x8, 0x1034f4000 <_global_added_lock+0x68>
100decef4:     	add	x8, x8, #0xef8
100decef8:     	ldr	x8, [x8]
100decefc:     	mov	w9, #-0x3               ; =-3
100decf00:     	and	x9, x23, x9
100decf04:     	ldrb	w8, [x8, x9]
100decf08:     	cmp	w8, #0x24
100decf0c:     	b.ne	0x100decf18 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2958>
100decf10:     	mov	x0, x23
100decf14:     	bl	0x100c11200 <__RINvMs0_NtCseKsKcgsFYGN_8otter_gc4heapNtB6_6GcHeap12with_payloadNtNtCsgTc3P8JF8aG_8otter_vm14iterator_state13IteratorStateuNCINvMNtB14_12iterator_opsNtB14_11Interpreter19drive_iterator_nextNtNtB14_10executable11OperandViewEs0_0EB14_>
100decf18:     	mov	w21, #0xa               ; =10
100decf1c:     	ldr	x28, [sp, #0x348]
100decf20:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100decf24:     	add	x2, x2, #0x488
100decf28:     	ldr	x0, [sp, #0x328]
100decf2c:     	mov	x1, x28
100decf30:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100decf34:     	mov	x22, #0x0               ; =0
100decf38:     	ldrh	w8, [x0, #0x8]
100decf3c:     	mov	w26, #0x1               ; =1
100decf40:     	ldr	x10, [sp, #0x268]
100decf44:     	cmp	w8, w10, uxth
100decf48:     	b.ls	0x100df0ed0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6910>
100decf4c:     	ldr	x9, [x0, #0x10]
100decf50:     	mov	w28, #0x1               ; =1
100decf54:     	cbz	x9, 0x100df1d08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7748>
100decf58:     	str	x21, [x9, x10, lsl #3]
100decf5c:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100decf60:     	add	x2, x2, #0x4a0
100decf64:     	ldr	x0, [sp, #0x328]
100decf68:     	ldr	x1, [sp, #0x348]
100decf6c:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100decf70:     	mov	x22, #0x0               ; =0
100decf74:     	ldrh	w8, [x0, #0x8]
100decf78:     	mov	w26, #0x1               ; =1
100decf7c:     	cmp	w8, w27
100decf80:     	b.ls	0x100df0ed0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6910>
100decf84:     	ldr	x9, [x0, #0x10]
100decf88:     	mov	w28, #0x1               ; =1
100decf8c:     	cbz	x9, 0x100df1d08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7748>
100decf90:     	eor	x8, x19, #0x7
100decf94:     	ldr	x10, [sp, #0x270]
100decf98:     	str	x8, [x9, x10, lsl #3]
100decf9c:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100decfa0:     	add	x2, x2, #0x4b8
100decfa4:     	ldr	x0, [sp, #0x328]
100decfa8:     	ldr	x19, [sp, #0x348]
100decfac:     	mov	x1, x19
100decfb0:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100decfb4:     	ldr	w8, [x0, #0x64]
100decfb8:     	ldr	x0, [sp, #0x328]
100decfbc:     	cbz	w8, 0x100decfe0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2a20>
100decfc0:     	ldr	x1, [x20, #0x7558]
100decfc4:     	sub	x23, x8, #0x1
100decfc8:     	cmp	x23, x1
100decfcc:     	b.hs	0x100df1ea4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x78e4>
100decfd0:     	ldr	x8, [x20, #0x7550]
100decfd4:     	mov	w9, #0xd0               ; =208
100decfd8:     	mul	x9, x23, x9
100decfdc:     	str	xzr, [x8, x9]
100decfe0:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100decfe4:     	add	x2, x2, #0x4d0
100decfe8:     	mov	x1, x19
100decfec:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100decff0:     	ldr	w8, [x0, #0x4]
100decff4:     	cmn	w8, #0x1
100decff8:     	b.ne	0x100decd64 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x27a4>
100decffc:     	b	0x100df0ea0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68e0>
100ded000:     	add	x0, sp, #0x420
100ded004:     	sub	x4, x29, #0x84
100ded008:     	mov	x1, x20
100ded00c:     	ldr	x2, [sp, #0x308]
100ded010:     	ldr	x3, [sp, #0x328]
100ded014:     	bl	0x100ce8880 <__RNvMNtCsgTc3P8JF8aG_8otter_vm12iterator_opsNtB4_11Interpreter18iterator_next_full>
100ded018:     	ldr	x26, [sp, #0x420]
100ded01c:     	ldr	x19, [sp, #0x428]
100ded020:     	ldrb	w21, [sp, #0x430]
100ded024:     	cmp	x26, #0x2
100ded028:     	b.ne	0x100df1cb0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x76f0>
100ded02c:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100ded030:     	add	x2, x2, #0x3b0
100ded034:     	ldr	x0, [sp, #0x328]
100ded038:     	ldr	x1, [sp, #0x348]
100ded03c:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100ded040:     	mov	x22, #0x0               ; =0
100ded044:     	ldrh	w8, [x0, #0x8]
100ded048:     	mov	w28, #0x1               ; =1
100ded04c:     	ldr	x9, [sp, #0x268]
100ded050:     	cmp	w8, w9, uxth
100ded054:     	b.ls	0x100df1ce8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7728>
100ded058:     	ldr	x9, [x0, #0x10]
100ded05c:     	mov	w26, #0x1               ; =1
100ded060:     	ldr	x0, [sp, #0x328]
100ded064:     	ldr	x10, [sp, #0x268]
100ded068:     	mov	x23, x21
100ded06c:     	cbz	x9, 0x100df13e0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e20>
100ded070:     	str	x19, [x9, x10, lsl #3]
100ded074:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100ded078:     	add	x2, x2, #0x3c8
100ded07c:     	ldr	x1, [sp, #0x348]
100ded080:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100ded084:     	mov	x22, #0x0               ; =0
100ded088:     	ldrh	w8, [x0, #0x8]
100ded08c:     	mov	w28, #0x1               ; =1
100ded090:     	ldr	w9, [sp, #0x258]
100ded094:     	cmp	w8, w9
100ded098:     	b.ls	0x100df1ce8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7728>
100ded09c:     	ldr	x9, [x0, #0x10]
100ded0a0:     	mov	w26, #0x1               ; =1
100ded0a4:     	ldr	x0, [sp, #0x328]
100ded0a8:     	mov	x23, x21
100ded0ac:     	cbz	x9, 0x100df13e0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e20>
100ded0b0:     	tst	w23, #0x1
100ded0b4:     	mov	w8, #0x6                ; =6
100ded0b8:     	cinc	x8, x8, ne
100ded0bc:     	ldr	x10, [sp, #0x270]
100ded0c0:     	str	x8, [x9, x10, lsl #3]
100ded0c4:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100ded0c8:     	add	x2, x2, #0x3e0
100ded0cc:     	ldr	x1, [sp, #0x348]
100ded0d0:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100ded0d4:     	b	0x100debed0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1910>
100ded0d8:     	scvtf	d0, w8
100ded0dc:     	scvtf	d1, w7
100ded0e0:     	fsub	d0, d0, d1
100ded0e4:     	fcvtzs	w9, d0
100ded0e8:     	scvtf	d1, w9
100ded0ec:     	fcmp	d0, d1
100ded0f0:     	b.eq	0x100deae54 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x894>
100ded0f4:     	fmov	x10, d0
100ded0f8:     	mov	x9, #0x2000000000000    ; =562949953421312
100ded0fc:     	add	x9, x10, x9
100ded100:     	fcmp	d0, d0
100ded104:     	mov	x10, #0x7ffa000000000000 ; =9221683186994511872
100ded108:     	csel	x9, x10, x9, vs
100ded10c:     	cbz	x28, 0x100ded204 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c44>
100ded110:     	mov	x10, #-0x2000000000001  ; =-562949953421313
100ded114:     	cmp	x8, x10
100ded118:     	mov	w8, #0x3                ; =3
100ded11c:     	csinc	w8, w8, wzr, ls
100ded120:     	ldsetb	w8, w10, [x26]
100ded124:     	bics	wzr, w8, w10
100ded128:     	b.eq	0x100ded204 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c44>
100ded12c:     	ldr	w8, [x28, #0x20]
100ded130:     	cmn	w8, #0x1
100ded134:     	b.eq	0x100ded204 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c44>
100ded138:     	add	w10, w8, #0x1
100ded13c:     	add	x11, x28, #0x20
100ded140:     	mov	x12, x8
100ded144:     	casl	w12, w10, [x11]
100ded148:     	cmp	w12, w8
100ded14c:     	mov	x8, x12
100ded150:     	b.ne	0x100ded130 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2b70>
100ded154:     	b	0x100ded204 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c44>
100ded158:     	scvtf	d0, w9
100ded15c:     	scvtf	d1, w7
100ded160:     	fadd	d0, d1, d0
100ded164:     	fcvtzs	w8, d0
100ded168:     	scvtf	d1, w8
100ded16c:     	fcmp	d0, d1
100ded170:     	b.ne	0x100ded22c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c6c>
100ded174:     	cbnz	w8, 0x100ded180 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2bc0>
100ded178:     	fmov	x10, d0
100ded17c:     	tbnz	x10, #0x3f, 0x100ded230 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c70>
100ded180:     	orr	x8, x8, #0xfffe000000000000
100ded184:     	b	0x100ded238 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c78>
100ded188:     	scvtf	d0, w8
100ded18c:     	scvtf	d1, w7
100ded190:     	fadd	d0, d0, d1
100ded194:     	fcvtzs	w9, d0
100ded198:     	scvtf	d1, w9
100ded19c:     	fcmp	d0, d1
100ded1a0:     	b.eq	0x100deb2cc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0xd0c>
100ded1a4:     	fmov	x10, d0
100ded1a8:     	mov	x9, #0x2000000000000    ; =562949953421312
100ded1ac:     	add	x9, x10, x9
100ded1b0:     	fcmp	d0, d0
100ded1b4:     	mov	x10, #0x7ffa000000000000 ; =9221683186994511872
100ded1b8:     	csel	x9, x10, x9, vs
100ded1bc:     	cbz	x28, 0x100ded204 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c44>
100ded1c0:     	mov	x10, #-0x2000000000001  ; =-562949953421313
100ded1c4:     	cmp	x8, x10
100ded1c8:     	mov	w8, #0x3                ; =3
100ded1cc:     	csinc	w8, w8, wzr, ls
100ded1d0:     	ldsetb	w8, w10, [x26]
100ded1d4:     	bics	wzr, w8, w10
100ded1d8:     	b.eq	0x100ded204 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c44>
100ded1dc:     	ldr	w8, [x28, #0x20]
100ded1e0:     	cmn	w8, #0x1
100ded1e4:     	b.eq	0x100ded204 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c44>
100ded1e8:     	add	w10, w8, #0x1
100ded1ec:     	add	x11, x28, #0x20
100ded1f0:     	mov	x12, x8
100ded1f4:     	casl	w12, w10, [x11]
100ded1f8:     	cmp	w12, w8
100ded1fc:     	mov	x8, x12
100ded200:     	b.ne	0x100ded1e0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c20>
100ded204:     	ldr	x8, [sp, #0x358]
100ded208:     	ldr	x10, [x8, #0x10]
100ded20c:     	str	x9, [x10, x5, lsl #3]
100ded210:     	ldr	w9, [x8, #0x4]
100ded214:     	add	w9, w9, #0x1
100ded218:     	str	w9, [x8, #0x4]
100ded21c:     	ldr	x8, [sp, #0x358]
100ded220:     	b	0x100dea894 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2d4>
100ded224:     	scvtf	d0, w10
100ded228:     	b	0x100deb814 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1254>
100ded22c:     	fmov	x10, d0
100ded230:     	mov	x8, #0x2000000000000    ; =562949953421312
100ded234:     	add	x8, x10, x8
100ded238:     	ldr	x19, [sp, #0x308]
100ded23c:     	ldr	w22, [sp, #0x348]
100ded240:     	cbnz	x28, 0x100dec9f0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2430>
100ded244:     	b	0x100decc04 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2644>
100ded248:     	bl	0x101b64e08 <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm10native_abi5frameNtB5_5Frame20derive_logical_depth>
100ded24c:     	mov	x8, x0
100ded250:     	ldr	x9, [sp, #0x328]
100ded254:     	b	0x100decec8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2908>
100ded258:     	lsl	w8, w8, #16
100ded25c:     	mov	w9, w22
100ded260:     	orr	x9, x9, x10, lsl #32
100ded264:     	orr	x27, x9, x8
100ded268:     	mov	x0, x24
100ded26c:     	bl	0x100f16450 <__RNvNtCslDZdWM0QmzH_14otter_bytecode16scalar_semantics24numeric_binary_semantics>
100ded270:     	and	w8, w0, #0xff
100ded274:     	cmp	w8, #0x2
100ded278:     	ldr	x2, [sp, #0x328]
100ded27c:     	b.eq	0x100df1ddc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x781c>
100ded280:     	lsr	w6, w0, #24
100ded284:     	str	x26, [sp]
100ded288:     	add	x0, sp, #0x420
100ded28c:     	mov	x1, x20
100ded290:     	mov	x3, x19
100ded294:     	mov	x4, x23
100ded298:     	mov	x5, x27
100ded29c:     	mov	x7, x28
100ded2a0:     	bl	0x100e377b0 <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB6_11Interpreter23run_numeric_binary_regs>
100ded2a4:     	ldr	x8, [sp, #0x420]
100ded2a8:     	cmp	x8, #0x2
100ded2ac:     	b.eq	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100ded2b0:     	b	0x100df136c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6dac>
100ded2b4:     	ldrh	w23, [x27, #0xc]
100ded2b8:     	ldrh	w6, [x27, #0x10]
100ded2bc:     	ldrh	w7, [x27, #0x14]
100ded2c0:     	cmp	w24, #0x8d
100ded2c4:     	b.le	0x100ded328 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2d68>
100ded2c8:     	cmp	w24, #0x8e
100ded2cc:     	ldr	x2, [sp, #0x328]
100ded2d0:     	b.eq	0x100def9fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x543c>
100ded2d4:     	cmp	w24, #0x8f
100ded2d8:     	b.eq	0x100defa7c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x54bc>
100ded2dc:     	cmp	w24, #0x90
100ded2e0:     	b.ne	0x100df1e38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7878>
100ded2e4:     	ldr	x4, [sp, #0x358]
100ded2e8:     	stp	x28, x26, [sp, #0x8]
100ded2ec:     	mov	w8, #0x1                ; =1
100ded2f0:     	strb	w8, [sp]
100ded2f4:     	add	x0, sp, #0x420
100ded2f8:     	mov	x1, x20
100ded2fc:     	mov	x3, x19
100ded300:     	mov	x5, x23
100ded304:     	bl	0x100e371ac <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB6_11Interpreter20run_loose_equal_regs>
100ded308:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100ded30c:     	stp	x28, x26, [sp, #0x8]
100ded310:     	add	x0, sp, #0x420
100ded314:     	strb	w24, [sp]
100ded318:     	mov	x1, x20
100ded31c:     	mov	x3, x19
100ded320:     	bl	0x100e361cc <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB6_11Interpreter16run_compare_regs>
100ded324:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100ded328:     	cmp	w24, #0x1f
100ded32c:     	b.eq	0x100def9d0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5410>
100ded330:     	cmp	w24, #0x20
100ded334:     	b.ne	0x100df1e38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7878>
100ded338:     	ldr	x2, [sp, #0x358]
100ded33c:     	str	x26, [sp]
100ded340:     	add	x0, sp, #0x420
100ded344:     	mov	x1, x20
100ded348:     	mov	x3, x23
100ded34c:     	mov	x4, x6
100ded350:     	mov	x5, x7
100ded354:     	mov	w6, #0x1                ; =1
100ded358:     	mov	x7, x28
100ded35c:     	bl	0x100e35cd0 <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB6_11Interpreter14run_equal_regs>
100ded360:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100ded364:     	ldr	x8, [sp, #0x340]
100ded368:     	cbz	x8, 0x100defbdc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x561c>
100ded36c:     	ldr	w1, [x27]
100ded370:     	mov	x0, x25
100ded374:     	bl	0x100cf6e08 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter28record_call_attempt_feedback>
100ded378:     	add	x8, sp, #0x360
100ded37c:     	ldr	x21, [sp, #0x328]
100ded380:     	str	x21, [sp, #0x420]
100ded384:     	str	x8, [sp, #0x428]
100ded388:     	str	x25, [sp, #0x430]
100ded38c:     	str	x27, [sp, #0x438]
100ded390:     	str	x20, [sp, #0x440]
100ded394:     	cmp	w24, #0x42
100ded398:     	b.ne	0x100defca4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x56e4>
100ded39c:     	add	x0, sp, #0x420
100ded3a0:     	bl	0x100ca997c <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickss1_0Bb_>
100ded3a4:     	and	w19, w0, #0xff
100ded3a8:     	lsr	w8, w0, #8
100ded3ac:     	str	x8, [sp, #0x2c0]
100ded3b0:     	lsr	x8, x0, #32
100ded3b4:     	str	x8, [sp, #0x2b8]
100ded3b8:     	ldr	x21, [sp, #0x328]
100ded3bc:     	b	0x100defca8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x56e8>
100ded3c0:     	ldrh	w8, [x27, #0xc]
100ded3c4:     	ldr	x9, [sp, #0x358]
100ded3c8:     	ldr	w10, [x9, #0x18]
100ded3cc:     	cmp	w10, w8
100ded3d0:     	b.ls	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100ded3d4:     	ldr	x9, [x9, #0x10]
100ded3d8:     	ldr	x4, [x9, x8, lsl #3]
100ded3dc:     	ldr	x8, [sp, #0x328]
100ded3e0:     	ldr	x2, [x8, #0x180]
100ded3e4:     	add	x0, sp, #0x420
100ded3e8:     	mov	x1, x20
100ded3ec:     	ldr	x3, [sp, #0x2c8]
100ded3f0:     	mov	x5, #0x0                ; =0
100ded3f4:     	bl	0x100e4b284 <__RNvMs_NtNtCsgTc3P8JF8aG_8otter_vm6interp6framesNtB8_11Interpreter15pop_frame_above>
100ded3f8:     	b	0x100dee79c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x41dc>
100ded3fc:     	ldr	x3, [sp, #0x358]
100ded400:     	add	x0, sp, #0x420
100ded404:     	mov	x1, x20
100ded408:     	mov	x2, x24
100ded40c:     	mov	x4, x25
100ded410:     	mov	x5, x27
100ded414:     	bl	0x100c04c34 <__RINvMNtCsgTc3P8JF8aG_8otter_vm15static_call_opsNtB5_11Interpreter24run_static_call_operandsNtNtB5_10executable11OperandViewEB5_>
100ded418:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100ded41c:     	ldrh	w8, [x27, #0x10]
100ded420:     	ldr	x19, [sp, #0x358]
100ded424:     	ldr	w9, [x19, #0x18]
100ded428:     	cmp	w9, w8
100ded42c:     	b.ls	0x100df1550 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6f90>
100ded430:     	ldrh	w21, [x27, #0xc]
100ded434:     	ldr	w4, [x27, #0x14]
100ded438:     	ldr	x9, [x19, #0x10]
100ded43c:     	add	x8, x9, x8, lsl #3
100ded440:     	str	x8, [sp, #0x208]
100ded444:     	ldr	x3, [x8]
100ded448:     	cmp	w24, #0x3c
100ded44c:     	cset	w5, eq
100ded450:     	add	x0, sp, #0x420
100ded454:     	mov	x1, x20
100ded458:     	ldr	x2, [sp, #0x308]
100ded45c:     	bl	0x100cd6d34 <__RNvMNtCsgTc3P8JF8aG_8otter_vm11context_opsNtB4_11Interpreter23load_context_slot_value>
100ded460:     	ldrb	w8, [sp, #0x420]
100ded464:     	cmp	w8, #0xff
100ded468:     	b.ne	0x100df1494 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6ed4>
100ded46c:     	ldrh	w9, [x19, #0x8]
100ded470:     	ldr	x8, [x19, #0x10]
100ded474:     	cmp	w9, w21
100ded478:     	ccmp	x8, #0x0, #0x4, hi
100ded47c:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100ded480:     	ldr	x9, [sp, #0x428]
100ded484:     	b	0x100df02d4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5d14>
100ded488:     	ldrh	w19, [x27, #0xc]
100ded48c:     	cmp	w24, #0x78
100ded490:     	b.ne	0x100def5f4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5034>
100ded494:     	ldr	x8, [sp, #0x358]
100ded498:     	ldrh	w9, [x27, #0x10]
100ded49c:     	ldr	w10, [x8, #0x18]
100ded4a0:     	cmp	w9, w10
100ded4a4:     	ldr	x3, [sp, #0x328]
100ded4a8:     	b.hs	0x100df1834 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7274>
100ded4ac:     	ldr	x8, [x8, #0x10]
100ded4b0:     	ldr	x6, [x8, x9, lsl #3]
100ded4b4:     	mov	w5, #0x1                ; =1
100ded4b8:     	b	0x100def600 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5040>
100ded4bc:     	ldrh	w8, [x27, #0xc]
100ded4c0:     	ldr	x9, [sp, #0x358]
100ded4c4:     	ldr	w11, [x9, #0x18]
100ded4c8:     	cmp	w11, w8
100ded4cc:     	b.ls	0x100df1450 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e90>
100ded4d0:     	ldrh	w10, [x27, #0x10]
100ded4d4:     	ldr	x12, [sp, #0x238]
100ded4d8:     	str	x12, [sp, #0x240]
100ded4dc:     	cmp	w11, w10
100ded4e0:     	b.ls	0x100df1450 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e90>
100ded4e4:     	ldr	w4, [x27, #0x14]
100ded4e8:     	ldr	x9, [x9, #0x10]
100ded4ec:     	add	x8, x9, x8, lsl #3
100ded4f0:     	str	x8, [sp, #0x240]
100ded4f4:     	ldr	x5, [x8]
100ded4f8:     	add	x8, x9, x10, lsl #3
100ded4fc:     	str	x8, [sp, #0x238]
100ded500:     	ldr	x3, [x8]
100ded504:     	cmp	w24, #0x3e
100ded508:     	cset	w6, eq
100ded50c:     	add	x0, sp, #0x420
100ded510:     	mov	x1, x20
100ded514:     	mov	x2, x19
100ded518:     	bl	0x100cd7548 <__RNvMNtCsgTc3P8JF8aG_8otter_vm11context_opsNtB4_11Interpreter24store_context_slot_value>
100ded51c:     	b	0x100def700 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5140>
100ded520:     	ldrh	w19, [x27, #0xc]
100ded524:     	ldrh	w22, [x27, #0x10]
100ded528:     	ldrh	w21, [x27, #0x14]
100ded52c:     	ldr	x8, [sp, #0x358]
100ded530:     	ldr	x23, [sp, #0x248]
100ded534:     	cbz	x28, 0x100ded564 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2fa4>
100ded538:     	ldr	w9, [x8, #0x18]
100ded53c:     	cmp	w9, w21
100ded540:     	b.ls	0x100df16b8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x70f8>
100ded544:     	ldr	x8, [x8, #0x10]
100ded548:     	add	x23, x8, x21, lsl #3
100ded54c:     	ldr	x2, [x23]
100ded550:     	mov	x0, x28
100ded554:     	mov	x1, x26
100ded558:     	mov	x3, x2
100ded55c:     	bl	0x100e17f3c <__RNvMs8_NtCsgTc3P8JF8aG_8otter_vm8feedbackNtB5_27InstructionFeedbackRecorder12record_arith>
100ded560:     	ldr	x8, [sp, #0x358]
100ded564:     	ldr	x9, [sp, #0x340]
100ded568:     	str	x23, [sp, #0x248]
100ded56c:     	cbz	x9, 0x100ded5cc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x300c>
100ded570:     	ldr	w9, [x8, #0x18]
100ded574:     	cmp	w9, w19
100ded578:     	b.ls	0x100df16cc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x710c>
100ded57c:     	cmp	w9, w22
100ded580:     	b.ls	0x100df16e0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7120>
100ded584:     	ldr	x8, [x8, #0x10]
100ded588:     	add	x9, x8, w19, uxtw #3
100ded58c:     	ldr	x23, [x9]
100ded590:     	add	x8, x8, x22, lsl #3
100ded594:     	stp	x8, x9, [sp, #0x198]
100ded598:     	ldr	x26, [x8]
100ded59c:     	ldr	w27, [x27]
100ded5a0:     	mov	x0, x25
100ded5a4:     	mov	x1, x27
100ded5a8:     	mov	x2, x23
100ded5ac:     	bl	0x100d0bbc0 <__RNvMNtCsgTc3P8JF8aG_8otter_vm16element_feedbackNtB4_11Interpreter30record_element_family_feedback>
100ded5b0:     	mov	x1, x25
100ded5b4:     	mov	x2, x27
100ded5b8:     	mov	x3, x23
100ded5bc:     	mov	x4, x26
100ded5c0:     	bl	0x100d0b9d4 <__RNvMNtCsgTc3P8JF8aG_8otter_vm16element_feedbackNtB4_11Interpreter30record_element_bounds_feedback>
100ded5c4:     	mov	w19, w19
100ded5c8:     	ldr	x8, [sp, #0x358]
100ded5cc:     	ldrh	w9, [x8, #0x8]
100ded5d0:     	ldr	x11, [x8, #0x10]
100ded5d4:     	cmp	x11, #0x0
100ded5d8:     	csel	x10, xzr, x9, eq
100ded5dc:     	ccmp	x9, #0x0, #0x4, ne
100ded5e0:     	mov	w9, #0x8                ; =8
100ded5e4:     	csel	x9, x9, x11, eq
100ded5e8:     	cmp	x10, x19
100ded5ec:     	ldr	x13, [sp, #0x230]
100ded5f0:     	ldr	x14, [sp, #0x228]
100ded5f4:     	ldr	x12, [sp, #0x220]
100ded5f8:     	b.ls	0x100df13e8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e28>
100ded5fc:     	mov	w11, w22
100ded600:     	mov	x13, x12
100ded604:     	cmp	x10, x11
100ded608:     	b.ls	0x100df13e8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e28>
100ded60c:     	mov	w12, w21
100ded610:     	mov	x13, x14
100ded614:     	cmp	x10, x12
100ded618:     	b.ls	0x100df13e8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e28>
100ded61c:     	ldr	w4, [x8]
100ded620:     	ldr	x5, [x9, x19, lsl #3]
100ded624:     	ldr	x6, [x9, x11, lsl #3]
100ded628:     	cmp	w24, #0x6e
100ded62c:     	cset	w8, eq
100ded630:     	ldr	x7, [x9, x12, lsl #3]
100ded634:     	strb	w8, [sp]
100ded638:     	add	x0, sp, #0x420
100ded63c:     	mov	x1, x20
100ded640:     	ldr	x2, [sp, #0x328]
100ded644:     	ldr	x3, [sp, #0x308]
100ded648:     	str	x5, [sp, #0x230]
100ded64c:     	str	x6, [sp, #0x220]
100ded650:     	str	x7, [sp, #0x228]
100ded654:     	bl	0x100d344f0 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm17property_dispatch11jit_runtimeNtB6_11Interpreter20store_element_values>
100ded658:     	b	0x100def300 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4d40>
100ded65c:     	ldrh	w8, [x27, #0x10]
100ded660:     	ldr	x9, [sp, #0x358]
100ded664:     	ldr	w10, [x9, #0x18]
100ded668:     	cmp	w10, w8
100ded66c:     	b.ls	0x100df153c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6f7c>
100ded670:     	ldrh	w19, [x27, #0xc]
100ded674:     	ldr	x9, [x9, #0x10]
100ded678:     	ldp	w6, w7, [x27, #0x14]
100ded67c:     	add	x8, x9, x8, lsl #3
100ded680:     	str	x8, [sp, #0x210]
100ded684:     	ldr	x5, [x8]
100ded688:     	cmp	w24, #0x9d
100ded68c:     	cset	w8, ne
100ded690:     	ldr	w4, [sp, #0x36c]
100ded694:     	strb	w8, [sp]
100ded698:     	add	x0, sp, #0x420
100ded69c:     	mov	x1, x20
100ded6a0:     	ldr	x2, [sp, #0x308]
100ded6a4:     	ldr	x3, [sp, #0x328]
100ded6a8:     	bl	0x100cd7120 <__RNvMNtCsgTc3P8JF8aG_8otter_vm11context_opsNtB4_11Interpreter24load_lookup_global_value>
100ded6ac:     	b	0x100def480 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4ec0>
100ded6b0:     	cbnz	w10, 0x100defb3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x557c>
100ded6b4:     	and	x10, x24, #0xffff
100ded6b8:     	cmp	w8, w10
100ded6bc:     	b.ls	0x100df1b04 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7544>
100ded6c0:     	add	x8, x9, x10, lsl #3
100ded6c4:     	str	x8, [sp, #0x58]
100ded6c8:     	ldr	x2, [x8]
100ded6cc:     	ldr	w1, [x27]
100ded6d0:     	mov	x0, x25
100ded6d4:     	bl	0x100d0bbc0 <__RNvMNtCsgTc3P8JF8aG_8otter_vm16element_feedbackNtB4_11Interpreter30record_element_family_feedback>
100ded6d8:     	str	wzr, [sp, #0x348]
100ded6dc:     	ldr	x19, [sp, #0x308]
100ded6e0:     	b	0x100defb40 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5580>
100ded6e4:     	ldp	w5, w6, [x27, #0xc]
100ded6e8:     	ldr	w7, [x27, #0x14]
100ded6ec:     	ldr	x4, [sp, #0x358]
100ded6f0:     	stp	x28, x26, [sp]
100ded6f4:     	add	x0, sp, #0x420
100ded6f8:     	mov	x1, x20
100ded6fc:     	ldr	x2, [sp, #0x328]
100ded700:     	mov	x3, x19
100ded704:     	bl	0x100e35990 <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB6_11Interpreter13run_ushr_regs>
100ded708:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100ded70c:     	ldp	w4, w5, [x27, #0xc]
100ded710:     	ldr	x3, [sp, #0x358]
100ded714:     	ldr	x8, [x19]
100ded718:     	ldr	x2, [x8, #0x20]
100ded71c:     	add	x0, sp, #0x420
100ded720:     	mov	x1, x20
100ded724:     	bl	0x100cd86d4 <__RNvMNtCsgTc3P8JF8aG_8otter_vm12constant_opsNtB4_11Interpreter19run_load_bigint_reg>
100ded728:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100ded72c:     	add	x0, sp, #0x420
100ded730:     	mov	x1, x20
100ded734:     	mov	x2, x19
100ded738:     	ldr	x3, [sp, #0x328]
100ded73c:     	mov	x4, x25
100ded740:     	mov	x5, x27
100ded744:     	bl	0x100c00594 <__RINvMNtCsgTc3P8JF8aG_8otter_vm11promise_opsNtB5_11Interpreter25run_promise_call_operandsNtNtB5_10executable11OperandViewEB5_>
100ded748:     	b	0x100defb2c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x556c>
100ded74c:     	ldrh	w8, [x27, #0x10]
100ded750:     	ldr	x9, [sp, #0x358]
100ded754:     	ldr	w10, [x9, #0x18]
100ded758:     	cmp	w10, w8
100ded75c:     	b.ls	0x100df18ac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x72ec>
100ded760:     	ldrh	w19, [x27, #0xc]
100ded764:     	ldr	x9, [x9, #0x10]
100ded768:     	add	x8, x9, x8, lsl #3
100ded76c:     	str	x8, [sp, #0x190]
100ded770:     	ldr	x5, [x8]
100ded774:     	add	x0, sp, #0x420
100ded778:     	mov	x1, x20
100ded77c:     	ldr	x2, [sp, #0x328]
100ded780:     	ldr	x3, [sp, #0x308]
100ded784:     	mov	w4, #0x6                ; =6
100ded788:     	mov	w6, #0xa                ; =10
100ded78c:     	mov	w7, #0xa                ; =10
100ded790:     	bl	0x100df66fc <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm18runtime_activation16committed_valuesNtB9_11Interpreter12scalar_value>
100ded794:     	b	0x100def480 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4ec0>
100ded798:     	ldr	w23, [x27, #0xc]
100ded79c:     	mov	w19, #0x1               ; =1
100ded7a0:     	mov	x0, x25
100ded7a4:     	mov	x1, x27
100ded7a8:     	mov	w2, #0x1                ; =1
100ded7ac:     	bl	0x100db4240 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock7operand>
100ded7b0:     	and	x8, x0, #0xffff
100ded7b4:     	cmp	x8, #0x1
100ded7b8:     	b.ne	0x100df17bc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x71fc>
100ded7bc:     	lsr	x6, x0, #32
100ded7c0:     	ldr	w7, [x27, #0x14]
100ded7c4:     	ldr	x4, [sp, #0x360]
100ded7c8:     	add	x0, sp, #0x420
100ded7cc:     	mov	x1, x20
100ded7d0:     	ldr	x2, [sp, #0x308]
100ded7d4:     	ldr	x3, [sp, #0x328]
100ded7d8:     	mov	x5, x23
100ded7dc:     	str	x6, [sp, #0x288]
100ded7e0:     	bl	0x100ccae54 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10global_opsNtB4_11Interpreter28run_store_global_checked_reg>
100ded7e4:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100ded7e8:     	add	x0, sp, #0x420
100ded7ec:     	mov	x1, x20
100ded7f0:     	ldr	x2, [sp, #0x328]
100ded7f4:     	mov	x3, x19
100ded7f8:     	mov	x4, x25
100ded7fc:     	mov	x5, x27
100ded800:     	mov	w6, #0x1                ; =1
100ded804:     	bl	0x100c08fb8 <__RINvMNtNtCsgTc3P8JF8aG_8otter_vm17property_dispatch7driversNtB7_11Interpreter20drive_store_propertyNtNtB7_10executable11OperandViewEB7_>
100ded808:     	ldr	x8, [sp, #0x420]
100ded80c:     	cmp	x8, #0x2
100ded810:     	b.ne	0x100df0ec4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6904>
100ded814:     	ldrb	w8, [sp, #0x428]
100ded818:     	tbnz	w8, #0x0, 0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100ded81c:     	ldr	w8, [x27, #0x10]
100ded820:     	ldr	x2, [sp, #0x308]
100ded824:     	ldr	x9, [x2]
100ded828:     	ldr	x9, [x9, #0x30]
100ded82c:     	ldr	x10, [x9, #0x18]
100ded830:     	cmp	x10, x8
100ded834:     	ldr	x3, [sp, #0x328]
100ded838:     	b.ls	0x100df13a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6de8>
100ded83c:     	ldr	x9, [x9, #0x10]
100ded840:     	add	x8, x9, x8, lsl #5
100ded844:     	ldr	x9, [x8]
100ded848:     	cmn	x9, #0x1
100ded84c:     	b.eq	0x100df13a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6de8>
100ded850:     	ldr	w5, [x27, #0xc]
100ded854:     	ldr	w9, [x27, #0x14]
100ded858:     	ldp	x6, x7, [x8, #0x8]
100ded85c:     	ldr	wzr, [x8, #0x18]
100ded860:     	ldr	x4, [sp, #0x360]
100ded864:     	mov	w8, #0x1                ; =1
100ded868:     	strb	w8, [sp, #0x2]
100ded86c:     	strh	w9, [sp]
100ded870:     	add	x0, sp, #0x420
100ded874:     	mov	x1, x20
100ded878:     	bl	0x100d315d4 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm17property_dispatch10propertiesNtB6_11Interpreter22run_store_property_reg>
100ded87c:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100ded880:     	str	x25, [sp, #0x420]
100ded884:     	str	x27, [sp, #0x428]
100ded888:     	add	x8, sp, #0x358
100ded88c:     	str	x8, [sp, #0x430]
100ded890:     	str	x20, [sp, #0x438]
100ded894:     	ldr	x8, [sp, #0x340]
100ded898:     	cbz	x8, 0x100deffe8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5a28>
100ded89c:     	add	x0, sp, #0x420
100ded8a0:     	bl	0x100ca9ea4 <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickss4_0Bb_>
100ded8a4:     	ldr	x8, [x20, #0x77f0]
100ded8a8:     	ldapurb	w8, [x8, #0x18]
100ded8ac:     	ldr	x21, [sp, #0x328]
100ded8b0:     	cbnz	w8, 0x100df1334 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6d74>
100ded8b4:     	lsr	x8, x0, #8
100ded8b8:     	str	x8, [sp, #0x1a8]
100ded8bc:     	and	w19, w0, #0xff
100ded8c0:     	ldr	w1, [x27]
100ded8c4:     	mov	x0, x25
100ded8c8:     	bl	0x100cf6e08 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter28record_call_attempt_feedback>
100ded8cc:     	b	0x100defffc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5a3c>
100ded8d0:     	ldrh	w10, [x27, #0x10]
100ded8d4:     	ldr	x8, [sp, #0x358]
100ded8d8:     	ldr	w9, [x8, #0x18]
100ded8dc:     	cmp	w9, w10
100ded8e0:     	b.ls	0x100df1708 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7148>
100ded8e4:     	ldrh	w19, [x27, #0xc]
100ded8e8:     	ldr	x9, [x8, #0x10]
100ded8ec:     	add	x21, x9, x10, lsl #3
100ded8f0:     	ldr	x4, [x21]
100ded8f4:     	lsr	x10, x4, #49
100ded8f8:     	ldr	x2, [sp, #0x328]
100ded8fc:     	cbnz	x10, 0x100ded928 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x3368>
100ded900:     	add	x0, sp, #0x420
100ded904:     	mov	x1, x20
100ded908:     	ldr	x3, [sp, #0x308]
100ded90c:     	bl	0x100ef8250 <__RNvNtCsgTc3P8JF8aG_8otter_vm6coerce19to_numeric_or_throw>
100ded910:     	ldr	x8, [sp, #0x420]
100ded914:     	cmp	x8, #0x2
100ded918:     	b.ne	0x100df0ec4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6904>
100ded91c:     	ldr	x4, [sp, #0x428]
100ded920:     	ldr	x8, [sp, #0x358]
100ded924:     	ldr	x9, [x8, #0x10]
100ded928:     	ldrh	w10, [x8, #0x8]
100ded92c:     	cmp	x9, #0x0
100ded930:     	ccmp	w10, w19, #0x0, ne
100ded934:     	b.ls	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100ded938:     	str	x21, [sp, #0xa8]
100ded93c:     	str	x4, [x9, x19, lsl #3]
100ded940:     	b	0x100def710 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5150>
100ded944:     	ldp	w5, w6, [x27, #0xc]
100ded948:     	ldr	w7, [x27, #0x14]
100ded94c:     	ldr	x4, [sp, #0x360]
100ded950:     	add	x0, sp, #0x420
100ded954:     	mov	x1, x20
100ded958:     	mov	x2, x19
100ded95c:     	ldr	x3, [sp, #0x328]
100ded960:     	bl	0x100d000c0 <__RNvMNtCsgTc3P8JF8aG_8otter_vm15jit_private_opsNtB4_11Interpreter19run_private_set_reg>
100ded964:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100ded968:     	ldrh	w8, [x27, #0x10]
100ded96c:     	ldr	x9, [sp, #0x358]
100ded970:     	ldr	w10, [x9, #0x18]
100ded974:     	cmp	w10, w8
100ded978:     	b.ls	0x100df16f4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7134>
100ded97c:     	ldr	w24, [x27, #0xc]
100ded980:     	ldr	x10, [x9, #0x10]
100ded984:     	add	x10, x10, x8, lsl #3
100ded988:     	ldr	x26, [x10]
100ded98c:     	ldr	w8, [x9, #0x64]
100ded990:     	str	x10, [sp, #0x150]
100ded994:     	cbz	w8, 0x100defc20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5660>
100ded998:     	ldr	x1, [x20, #0x7558]
100ded99c:     	sub	x23, x8, #0x1
100ded9a0:     	cmp	x23, x1
100ded9a4:     	b.hs	0x100df1e84 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x78c4>
100ded9a8:     	ldr	x8, [x20, #0x7550]
100ded9ac:     	mov	w9, #0xd0               ; =208
100ded9b0:     	madd	x8, x23, x9, x8
100ded9b4:     	ldr	w9, [x8, #0xa0]
100ded9b8:     	cbz	w9, 0x100defc20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5660>
100ded9bc:     	ldr	w8, [x8, #0xa4]
100ded9c0:     	adrp	x9, 0x1034f4000 <_global_added_lock+0x68>
100ded9c4:     	add	x9, x9, #0xef8
100ded9c8:     	ldr	x9, [x9]
100ded9cc:     	orr	x19, x9, x8
100ded9d0:     	b	0x100defc24 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5664>
100ded9d4:     	str	x25, [sp, #0x420]
100ded9d8:     	str	x27, [sp, #0x428]
100ded9dc:     	add	x8, sp, #0x358
100ded9e0:     	str	x8, [sp, #0x430]
100ded9e4:     	str	x20, [sp, #0x438]
100ded9e8:     	ldr	x8, [sp, #0x340]
100ded9ec:     	cbz	x8, 0x100df0058 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5a98>
100ded9f0:     	add	x0, sp, #0x420
100ded9f4:     	bl	0x100ca9ea4 <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickss4_0Bb_>
100ded9f8:     	and	w19, w0, #0xff
100ded9fc:     	lsr	x8, x0, #8
100deda00:     	str	x8, [sp, #0x1b0]
100deda04:     	b	0x100df005c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5a9c>
100deda08:     	ldr	w5, [x27, #0xc]
100deda0c:     	ldr	x4, [sp, #0x360]
100deda10:     	add	x0, sp, #0x420
100deda14:     	mov	x1, x20
100deda18:     	mov	x2, x19
100deda1c:     	ldr	x3, [sp, #0x328]
100deda20:     	mov	w6, #0x1                ; =1
100deda24:     	bl	0x100e325b4 <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm15iterator_recordNtB6_11Interpreter21close_iterator_record>
100deda28:     	b	0x100def300 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4d40>
100deda2c:     	ldr	w4, [x27, #0xc]
100deda30:     	ldr	x3, [sp, #0x360]
100deda34:     	add	x0, sp, #0x420
100deda38:     	mov	x1, x20
100deda3c:     	ldr	x2, [sp, #0x328]
100deda40:     	bl	0x100d29f94 <__RNvMNtCsgTc3P8JF8aG_8otter_vm9class_opsNtB4_11Interpreter23run_bind_this_value_reg>
100deda44:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100deda48:     	mov	x0, x25
100deda4c:     	mov	x1, x27
100deda50:     	mov	x2, #0x0                ; =0
100deda54:     	bl	0x100db41e8 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock3reg>
100deda58:     	mov	x23, x0
100deda5c:     	mov	x0, x25
100deda60:     	mov	x1, x27
100deda64:     	mov	w2, #0x1                ; =1
100deda68:     	bl	0x100db41e8 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock3reg>
100deda6c:     	ldrb	w8, [x27, #0x1d]
100deda70:     	cmp	w8, #0x3
100deda74:     	ldr	x2, [sp, #0x308]
100deda78:     	b.lo	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100deda7c:     	cmp	w8, #0x5
100deda80:     	b.lo	0x100df19f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7438>
100deda84:     	ldr	x11, [x25, #0x88]
100deda88:     	ldr	w10, [x27, #0xc]
100deda8c:     	add	x8, x10, #0x2
100deda90:     	cmp	x8, x11
100deda94:     	b.hs	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100deda98:     	add	x9, x10, #0x3
100deda9c:     	cmp	x9, x11
100dedaa0:     	b.hs	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100dedaa4:     	add	x10, x10, #0x4
100dedaa8:     	cmp	x10, x11
100dedaac:     	b.hs	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100dedab0:     	ldr	x11, [sp, #0x358]
100dedab4:     	ldr	w12, [x11, #0x18]
100dedab8:     	cmp	w12, w23, uxth
100dedabc:     	ldp	x14, x13, [sp, #0x1b8]
100dedac0:     	b.ls	0x100df13e8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e28>
100dedac4:     	cmp	w12, w0, uxth
100dedac8:     	mov	x13, x14
100dedacc:     	b.ls	0x100df13e8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e28>
100dedad0:     	ldr	x12, [x25, #0x80]
100dedad4:     	ldr	w5, [x12, x8, lsl #2]
100dedad8:     	ldr	w6, [x12, x9, lsl #2]
100dedadc:     	ldr	w7, [x12, x10, lsl #2]
100dedae0:     	ldr	x8, [x11, #0x10]
100dedae4:     	add	x9, x8, w23, uxth #3
100dedae8:     	str	x9, [sp, #0x1c0]
100dedaec:     	ldr	x9, [x9]
100dedaf0:     	add	x8, x8, w0, uxth #3
100dedaf4:     	str	x8, [sp, #0x1b8]
100dedaf8:     	ldr	x4, [x8]
100dedafc:     	ldr	w3, [sp, #0x36c]
100dedb00:     	str	x9, [sp]
100dedb04:     	add	x0, sp, #0x420
100dedb08:     	mov	x1, x20
100dedb0c:     	bl	0x100cd6e20 <__RNvMNtCsgTc3P8JF8aG_8otter_vm11context_opsNtB4_11Interpreter23store_lookup_slot_value>
100dedb10:     	b	0x100deeacc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x450c>
100dedb14:     	ldr	w3, [x27, #0xc]
100dedb18:     	ldr	x2, [sp, #0x358]
100dedb1c:     	add	x0, sp, #0x420
100dedb20:     	mov	x1, x20
100dedb24:     	bl	0x100cc72d8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10global_opsNtB4_11Interpreter24run_load_global_this_reg>
100dedb28:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100dedb2c:     	ldp	w5, w6, [x27, #0xc]
100dedb30:     	ldr	x4, [sp, #0x360]
100dedb34:     	add	x0, sp, #0x420
100dedb38:     	mov	x1, x20
100dedb3c:     	mov	x2, x19
100dedb40:     	ldr	x3, [sp, #0x328]
100dedb44:     	bl	0x100cd7df8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm11promise_opsNtB4_11Interpreter29run_promise_fulfilled_of_regs>
100dedb48:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100dedb4c:     	str	x25, [sp, #0x420]
100dedb50:     	str	x27, [sp, #0x428]
100dedb54:     	add	x8, sp, #0x358
100dedb58:     	str	x8, [sp, #0x430]
100dedb5c:     	ldr	x8, [sp, #0x340]
100dedb60:     	cbz	x8, 0x100df00b4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5af4>
100dedb64:     	add	x0, sp, #0x420
100dedb68:     	bl	0x100caa868 <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickssi_0Bb_>
100dedb6c:     	mov	x23, x0
100dedb70:     	mov	x0, x25
100dedb74:     	mov	x1, x27
100dedb78:     	mov	w2, #0x1                ; =1
100dedb7c:     	bl	0x100db4240 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock7operand>
100dedb80:     	tst	x0, #0xffff
100dedb84:     	ldr	x1, [sp, #0x328]
100dedb88:     	b.ne	0x100defd78 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x57b8>
100dedb8c:     	ldr	x8, [sp, #0x358]
100dedb90:     	ldr	w9, [x8, #0x18]
100dedb94:     	cmp	w9, w0, lsr #16
100dedb98:     	b.ls	0x100defd78 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x57b8>
100dedb9c:     	ldr	x8, [x8, #0x10]
100dedba0:     	cbz	x8, 0x100df0b68 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x65a8>
100dedba4:     	lsr	w9, w0, #16
100dedba8:     	ldr	x0, [x8, x9, lsl #3]
100dedbac:     	bl	0x100caa784 <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickssh_0Bb_>
100dedbb0:     	mov	x24, x0
100dedbb4:     	mov	x26, x1
100dedbb8:     	str	xzr, [sp, #0x298]
100dedbbc:     	b	0x100df00bc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5afc>
100dedbc0:     	add	x0, sp, #0x420
100dedbc4:     	mov	x1, x20
100dedbc8:     	ldr	x2, [sp, #0x328]
100dedbcc:     	mov	x3, x25
100dedbd0:     	mov	x4, x27
100dedbd4:     	bl	0x100c0661c <__RINvMNtCsgTc3P8JF8aG_8otter_vm15static_call_opsNtB5_11Interpreter37run_array_buffer_static_call_operandsNtNtB5_10executable11OperandViewEB5_>
100dedbd8:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100dedbdc:     	ldp	w5, w6, [x27, #0xc]
100dedbe0:     	ldr	x4, [sp, #0x360]
100dedbe4:     	add	x0, sp, #0x420
100dedbe8:     	mov	x1, x20
100dedbec:     	mov	x2, x19
100dedbf0:     	ldr	x3, [sp, #0x328]
100dedbf4:     	bl	0x100cfd0a8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm14allocation_opsNtB4_11Interpreter34run_new_finalization_registry_regs>
100dedbf8:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100dedbfc:     	ldp	w6, w7, [x27, #0xc]
100dedc00:     	ldr	x5, [sp, #0x360]
100dedc04:     	ldr	x2, [x19]
100dedc08:     	ldr	w3, [x19, #0x10]
100dedc0c:     	add	x0, sp, #0x420
100dedc10:     	mov	x1, x20
100dedc14:     	ldr	x4, [sp, #0x328]
100dedc18:     	bl	0x100d172d8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm19jit_class_value_opsNtB4_11Interpreter27run_get_template_object_reg>
100dedc1c:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100dedc20:     	add	x0, sp, #0x420
100dedc24:     	mov	x1, x20
100dedc28:     	ldr	x2, [sp, #0x328]
100dedc2c:     	mov	x3, x19
100dedc30:     	mov	x4, x25
100dedc34:     	mov	x5, x27
100dedc38:     	mov	w6, #0x0                ; =0
100dedc3c:     	bl	0x100c08fb8 <__RINvMNtNtCsgTc3P8JF8aG_8otter_vm17property_dispatch7driversNtB7_11Interpreter20drive_store_propertyNtNtB7_10executable11OperandViewEB7_>
100dedc40:     	ldr	x8, [sp, #0x420]
100dedc44:     	cmp	x8, #0x2
100dedc48:     	b.ne	0x100df0ec4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6904>
100dedc4c:     	ldrb	w8, [sp, #0x428]
100dedc50:     	tbnz	w8, #0x0, 0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100dedc54:     	ldr	w8, [x27, #0x10]
100dedc58:     	ldr	x2, [sp, #0x308]
100dedc5c:     	ldr	x9, [x2]
100dedc60:     	ldr	x9, [x9, #0x30]
100dedc64:     	ldr	x10, [x9, #0x18]
100dedc68:     	cmp	x10, x8
100dedc6c:     	ldr	x3, [sp, #0x328]
100dedc70:     	b.ls	0x100df13a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6de8>
100dedc74:     	ldr	x9, [x9, #0x10]
100dedc78:     	add	x8, x9, x8, lsl #5
100dedc7c:     	ldr	x9, [x8]
100dedc80:     	cmn	x9, #0x1
100dedc84:     	b.eq	0x100df13a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6de8>
100dedc88:     	ldr	w5, [x27, #0xc]
100dedc8c:     	ldr	w9, [x27, #0x14]
100dedc90:     	ldp	x6, x7, [x8, #0x8]
100dedc94:     	ldr	wzr, [x8, #0x18]
100dedc98:     	ldr	x4, [sp, #0x360]
100dedc9c:     	strb	wzr, [sp, #0x2]
100dedca0:     	strh	w9, [sp]
100dedca4:     	add	x0, sp, #0x420
100dedca8:     	mov	x1, x20
100dedcac:     	bl	0x100d315d4 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm17property_dispatch10propertiesNtB6_11Interpreter22run_store_property_reg>
100dedcb0:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100dedcb4:     	ldp	w6, w7, [x27, #0xc]
100dedcb8:     	ldr	x5, [sp, #0x360]
100dedcbc:     	ldr	x2, [x19]
100dedcc0:     	ldr	w3, [x19, #0x10]
100dedcc4:     	add	x0, sp, #0x420
100dedcc8:     	mov	x1, x20
100dedccc:     	ldr	x4, [sp, #0x328]
100dedcd0:     	bl	0x100cd2ff8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10module_opsNtB4_11Interpreter31run_module_namespace_object_reg>
100dedcd4:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100dedcd8:     	ldp	w6, w7, [x27, #0xc]
100dedcdc:     	ldr	w8, [x27, #0x14]
100dedce0:     	ldr	x5, [sp, #0x360]
100dedce4:     	ldr	x2, [x19]
100dedce8:     	ldr	w3, [x19, #0x10]
100dedcec:     	str	w8, [sp]
100dedcf0:     	add	x0, sp, #0x420
100dedcf4:     	mov	x1, x20
100dedcf8:     	ldr	x4, [sp, #0x328]
100dedcfc:     	bl	0x100ccf0d8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10module_opsNtB4_11Interpreter27run_load_import_binding_reg>
100dedd00:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100dedd04:     	str	x25, [sp, #0x420]
100dedd08:     	str	x27, [sp, #0x428]
100dedd0c:     	add	x8, sp, #0x358
100dedd10:     	str	x8, [sp, #0x430]
100dedd14:     	str	x20, [sp, #0x438]
100dedd18:     	ldr	x8, [sp, #0x340]
100dedd1c:     	cbz	x8, 0x100df0138 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5b78>
100dedd20:     	add	x0, sp, #0x420
100dedd24:     	bl	0x100caa628 <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickssf_0Bb_>
100dedd28:     	and	w19, w0, #0xff
100dedd2c:     	lsr	x8, x0, #8
100dedd30:     	str	x8, [sp, #0x2a8]
100dedd34:     	ldr	w1, [x27]
100dedd38:     	mov	x0, x25
100dedd3c:     	bl	0x100cf6e08 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter28record_call_attempt_feedback>
100dedd40:     	mov	x0, x25
100dedd44:     	mov	x1, x27
100dedd48:     	mov	w2, #0x1                ; =1
100dedd4c:     	bl	0x100db4240 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock7operand>
100dedd50:     	tst	x0, #0xffff
100dedd54:     	ldr	x1, [sp, #0x328]
100dedd58:     	b.eq	0x100df0540 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5f80>
100dedd5c:     	mov	w23, #0x0               ; =0
100dedd60:     	b	0x100df0688 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x60c8>
100dedd64:     	ldp	w4, w5, [x27, #0xc]
100dedd68:     	ldr	w6, [x27, #0x14]
100dedd6c:     	ldr	x3, [sp, #0x358]
100dedd70:     	add	x0, sp, #0x420
100dedd74:     	mov	x1, x20
100dedd78:     	mov	x2, x19
100dedd7c:     	bl	0x100cdbe08 <__RNvMNtCsgTc3P8JF8aG_8otter_vm12function_opsNtB4_11Interpreter21run_make_closure_regs>
100dedd80:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100dedd84:     	ldr	w8, [x27, #0x14]
100dedd88:     	ldr	x9, [x19]
100dedd8c:     	ldr	x9, [x9, #0x30]
100dedd90:     	ldr	x10, [x9, #0x18]
100dedd94:     	cmp	x10, x8
100dedd98:     	b.ls	0x100df1584 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6fc4>
100dedd9c:     	ldr	x9, [x9, #0x10]
100dedda0:     	add	x8, x9, x8, lsl #5
100dedda4:     	ldr	x9, [x8]
100dedda8:     	cmn	x9, #0x1
100deddac:     	b.eq	0x100df1584 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6fc4>
100deddb0:     	ldp	x19, x12, [x8, #0x8]
100deddb4:     	ldr	w11, [x8, #0x18]
100deddb8:     	stp	x19, x12, [x29, #-0xc0]
100deddbc:     	stur	w11, [x29, #-0xb0]
100deddc0:     	ldr	w8, [x27]
100deddc4:     	ldr	x9, [x25, #0xe8]
100deddc8:     	cmp	x9, x8
100deddcc:     	ldr	x3, [sp, #0x328]
100deddd0:     	str	w11, [sp, #0x184]
100deddd4:     	str	x12, [sp, #0x178]
100deddd8:     	b.ls	0x100defbe4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5624>
100dedddc:     	ldr	x9, [x25, #0xe0]
100dedde0:     	mov	w10, #0x18              ; =24
100dedde4:     	umaddl	x8, w8, w10, x9
100dedde8:     	ldrh	w9, [x8]
100deddec:     	sub	w9, w9, #0x1
100deddf0:     	cmp	w9, #0x1
100deddf4:     	b.hi	0x100defbe4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5624>
100deddf8:     	ldr	x9, [x8, #0x8]
100deddfc:     	ldrb	w8, [x9, #0xb0]
100dede00:     	cmp	w8, #0x0
100dede04:     	add	x8, x25, #0xd0
100dede08:     	csel	x8, x8, xzr, eq
100dede0c:     	b	0x100defbe8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5628>
100dede10:     	ldp	w5, w6, [x27, #0xc]
100dede14:     	ldr	x4, [sp, #0x360]
100dede18:     	add	x0, sp, #0x420
100dede1c:     	mov	x1, x20
100dede20:     	mov	x2, x19
100dede24:     	ldr	x3, [sp, #0x328]
100dede28:     	bl	0x100cc3b60 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10conversionNtB4_11Interpreter18run_to_number_regs>
100dede2c:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100dede30:     	ldrh	w8, [x27, #0x10]
100dede34:     	ldr	x19, [sp, #0x358]
100dede38:     	ldr	w9, [x19, #0x18]
100dede3c:     	cmp	w9, w8
100dede40:     	b.ls	0x100df1744 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7184>
100dede44:     	ldrh	w21, [x27, #0xc]
100dede48:     	ldp	w5, w6, [x27, #0x14]
100dede4c:     	ldr	x9, [x19, #0x10]
100dede50:     	add	x8, x9, x8, lsl #3
100dede54:     	str	x8, [sp, #0x120]
100dede58:     	ldr	x4, [x8]
100dede5c:     	ldr	w3, [sp, #0x36c]
100dede60:     	add	x0, sp, #0x420
100dede64:     	mov	x1, x20
100dede68:     	ldr	x2, [sp, #0x308]
100dede6c:     	bl	0x100cd69e4 <__RNvMNtCsgTc3P8JF8aG_8otter_vm11context_opsNtB4_11Interpreter22load_lookup_slot_value>
100dede70:     	ldrb	w8, [sp, #0x420]
100dede74:     	cmp	w8, #0xff
100dede78:     	b.ne	0x100df1464 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6ea4>
100dede7c:     	ldrh	w9, [x19, #0x8]
100dede80:     	ldr	x8, [x19, #0x10]
100dede84:     	cmp	w9, w21
100dede88:     	ccmp	x8, #0x0, #0x4, hi
100dede8c:     	b.ne	0x100def5dc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x501c>
100dede90:     	b	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100dede94:     	ldr	x19, [sp, #0x358]
100dede98:     	ldrh	w8, [x19, #0x8]
100dede9c:     	ldr	x9, [x19, #0x10]
100dedea0:     	cmp	x9, #0x0
100dedea4:     	csel	x3, xzr, x8, eq
100dedea8:     	ldp	w4, w5, [x27, #0xc]
100dedeac:     	ccmp	x8, #0x0, #0x4, ne
100dedeb0:     	mov	w8, #0x8                ; =8
100dedeb4:     	csel	x2, x8, x9, eq
100dedeb8:     	add	x0, sp, #0x420
100dedebc:     	mov	x1, x20
100dedec0:     	bl	0x100cd5590 <__RNvMNtCsgTc3P8JF8aG_8otter_vm11context_opsNtB4_11Interpreter18frame_copy_context>
100dedec4:     	ldrb	w8, [sp, #0x420]
100dedec8:     	cmp	w8, #0xff
100dedecc:     	b.eq	0x100df05c4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6004>
100deded0:     	b	0x100df0e20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6860>
100deded4:     	ldr	x8, [x25, #0xe8]
100deded8:     	cmp	x8, x21
100dededc:     	b.ls	0x100defd90 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x57d0>
100dedee0:     	ldr	x8, [x25, #0xe0]
100dedee4:     	mov	w9, #0x18               ; =24
100dedee8:     	madd	x8, x21, x9, x8
100dedeec:     	ldrh	w9, [x8], #0x2
100dedef0:     	cmp	w9, #0x4
100dedef4:     	csel	x1, x8, xzr, eq
100dedef8:     	b	0x100defd94 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x57d4>
100dedefc:     	ldrh	w8, [x27, #0x10]
100dedf00:     	ldr	x9, [sp, #0x358]
100dedf04:     	ldr	w10, [x9, #0x18]
100dedf08:     	cmp	w10, w8
100dedf0c:     	b.ls	0x100df17e0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7220>
100dedf10:     	ldrh	w19, [x27, #0xc]
100dedf14:     	ldr	x9, [x9, #0x10]
100dedf18:     	add	x8, x9, x8, lsl #3
100dedf1c:     	str	x8, [sp, #0x140]
100dedf20:     	ldr	x5, [x8]
100dedf24:     	add	x0, sp, #0x420
100dedf28:     	mov	x1, x20
100dedf2c:     	ldr	x2, [sp, #0x328]
100dedf30:     	ldr	x3, [sp, #0x308]
100dedf34:     	mov	w4, #0x0                ; =0
100dedf38:     	mov	w6, #0xa                ; =10
100dedf3c:     	mov	w7, #0xa                ; =10
100dedf40:     	bl	0x100df66fc <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm18runtime_activation16committed_valuesNtB9_11Interpreter12scalar_value>
100dedf44:     	b	0x100def480 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4ec0>
100dedf48:     	ldp	w5, w6, [x27, #0xc]
100dedf4c:     	ldr	x4, [sp, #0x360]
100dedf50:     	add	x0, sp, #0x420
100dedf54:     	mov	x1, x20
100dedf58:     	mov	x2, x19
100dedf5c:     	ldr	x3, [sp, #0x328]
100dedf60:     	bl	0x100d008dc <__RNvMNtCsgTc3P8JF8aG_8otter_vm15jit_private_opsNtB4_11Interpreter27run_private_brand_check_reg>
100dedf64:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100dedf68:     	ldrh	w8, [x27, #0xc]
100dedf6c:     	ldr	x9, [sp, #0x358]
100dedf70:     	ldr	w10, [x9, #0x18]
100dedf74:     	cmp	w10, w8
100dedf78:     	b.ls	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100dedf7c:     	ldr	x9, [x9, #0x10]
100dedf80:     	ldr	x23, [x9, x8, lsl #3]
100dedf84:     	ldr	x8, [sp, #0x328]
100dedf88:     	ldr	x3, [x8, #0x180]
100dedf8c:     	mov	w8, #0xff               ; =255
100dedf90:     	strb	w8, [sp, #0x420]
100dedf94:     	sub	x0, x29, #0xa8
100dedf98:     	add	x6, sp, #0x420
100dedf9c:     	mov	x1, x20
100dedfa0:     	mov	x2, x19
100dedfa4:     	ldr	x4, [sp, #0x2c8]
100dedfa8:     	mov	x5, x23
100dedfac:     	mov	w7, #0x0                ; =0
100dedfb0:     	bl	0x100d275ac <__RNvMNtCsgTc3P8JF8aG_8otter_vm9async_opsNtB4_11Interpreter32unwind_throw_with_uncaught_above>
100dedfb4:     	ldurb	w8, [x29, #-0xa8]
100dedfb8:     	cmp	w8, #0xff
100dedfbc:     	b.eq	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100dedfc0:     	b	0x100df19b4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x73f4>
100dedfc4:     	ldp	w5, w6, [x27, #0xc]
100dedfc8:     	ldr	w7, [x27, #0x14]
100dedfcc:     	ldr	x4, [sp, #0x360]
100dedfd0:     	add	x0, sp, #0x420
100dedfd4:     	mov	x1, x20
100dedfd8:     	mov	x2, x19
100dedfdc:     	ldr	x3, [sp, #0x328]
100dedfe0:     	bl	0x100d2ae18 <__RNvMNtCsgTc3P8JF8aG_8otter_vm9error_opsNtB4_11Interpreter18run_new_error_regs>
100dedfe4:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100dedfe8:     	ldr	x4, [sp, #0x360]
100dedfec:     	add	x0, sp, #0x420
100dedff0:     	mov	x1, x20
100dedff4:     	mov	x2, x19
100dedff8:     	ldr	x3, [sp, #0x328]
100dedffc:     	mov	x5, x25
100dee000:     	mov	x6, x27
100dee004:     	bl	0x100bfeda8 <__RINvMNtCsgTc3P8JF8aG_8otter_vm10module_opsNtB5_11Interpreter37run_import_namespace_dynamic_operandsNtNtB5_10executable11OperandViewEB5_>
100dee008:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100dee00c:     	ldrh	w8, [x27, #0xc]
100dee010:     	ldr	x9, [sp, #0x358]
100dee014:     	ldr	w11, [x9, #0x18]
100dee018:     	cmp	w11, w8
100dee01c:     	b.ls	0x100df15fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x703c>
100dee020:     	ldrh	w10, [x27, #0x10]
100dee024:     	ldr	x12, [sp, #0x1d0]
100dee028:     	str	x12, [sp, #0x1f0]
100dee02c:     	cmp	w11, w10
100dee030:     	b.ls	0x100df15fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x703c>
100dee034:     	ldp	w6, w7, [x27, #0x14]
100dee038:     	ldr	x9, [x9, #0x10]
100dee03c:     	add	x8, x9, x8, lsl #3
100dee040:     	str	x8, [sp, #0x1f0]
100dee044:     	ldr	x8, [x8]
100dee048:     	add	x9, x9, x10, lsl #3
100dee04c:     	str	x9, [sp, #0x1d0]
100dee050:     	ldr	x5, [x9]
100dee054:     	ldr	w4, [sp, #0x36c]
100dee058:     	str	x8, [sp]
100dee05c:     	add	x0, sp, #0x420
100dee060:     	mov	x1, x20
100dee064:     	mov	x2, x19
100dee068:     	ldr	x3, [sp, #0x328]
100dee06c:     	bl	0x100cd7860 <__RNvMNtCsgTc3P8JF8aG_8otter_vm11context_opsNtB4_11Interpreter25store_lookup_global_value>
100dee070:     	b	0x100def300 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4d40>
100dee074:     	ldrb	w8, [x27, #0x1d]
100dee078:     	cbz	w8, 0x100df0144 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5b84>
100dee07c:     	cmp	w8, #0x5
100dee080:     	ldr	x3, [sp, #0x328]
100dee084:     	b.hs	0x100df02dc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5d1c>
100dee088:     	ldr	w5, [x27, #0xc]
100dee08c:     	b	0x100df05e0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6020>
100dee090:     	ldr	w5, [x27, #0xc]
100dee094:     	ldr	x4, [sp, #0x360]
100dee098:     	add	x0, sp, #0x420
100dee09c:     	mov	x1, x20
100dee0a0:     	mov	x2, x19
100dee0a4:     	ldr	x3, [sp, #0x328]
100dee0a8:     	mov	w6, #0x0                ; =0
100dee0ac:     	bl	0x100e325b4 <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm15iterator_recordNtB6_11Interpreter21close_iterator_record>
100dee0b0:     	b	0x100def300 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4d40>
100dee0b4:     	ldp	w3, w4, [x27, #0xc]
100dee0b8:     	ldr	w5, [x27, #0x14]
100dee0bc:     	ldr	x2, [sp, #0x358]
100dee0c0:     	str	x26, [sp]
100dee0c4:     	add	x0, sp, #0x420
100dee0c8:     	mov	x1, x20
100dee0cc:     	mov	w6, #0x1                ; =1
100dee0d0:     	mov	x7, x28
100dee0d4:     	bl	0x100e35788 <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB6_11Interpreter13run_equal_imm>
100dee0d8:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100dee0dc:     	ldrh	w8, [x27, #0x10]
100dee0e0:     	ldr	x9, [sp, #0x358]
100dee0e4:     	ldr	w11, [x9, #0x18]
100dee0e8:     	cmp	w11, w8
100dee0ec:     	b.ls	0x100df1780 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x71c0>
100dee0f0:     	ldrh	w10, [x27, #0x14]
100dee0f4:     	cmp	w11, w10
100dee0f8:     	ldr	x3, [sp, #0x328]
100dee0fc:     	b.ls	0x100df1978 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x73b8>
100dee100:     	ldr	w5, [x27, #0xc]
100dee104:     	ldr	x9, [x9, #0x10]
100dee108:     	add	x8, x9, x8, lsl #3
100dee10c:     	str	x8, [sp, #0x110]
100dee110:     	ldr	x6, [x8]
100dee114:     	add	x8, x9, x10, lsl #3
100dee118:     	str	x8, [sp, #0xe0]
100dee11c:     	ldr	x8, [x8]
100dee120:     	str	x8, [sp, #0x428]
100dee124:     	mov	w8, #-0x1               ; =-1
100dee128:     	str	w8, [sp, #0x420]
100dee12c:     	ldr	x4, [sp, #0x360]
100dee130:     	sub	x0, x29, #0xa8
100dee134:     	add	x7, sp, #0x420
100dee138:     	mov	x1, x20
100dee13c:     	mov	x2, x19
100dee140:     	bl	0x100d0f2c0 <__RNvMNtCsgTc3P8JF8aG_8otter_vm17property_dispatchNtB4_11Interpreter23run_load_super_property>
100dee144:     	b	0x100def87c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x52bc>
100dee148:     	ldrh	w8, [x27, #0x10]
100dee14c:     	ldr	x9, [sp, #0x358]
100dee150:     	ldr	w11, [x9, #0x18]
100dee154:     	cmp	w11, w8
100dee158:     	b.ls	0x100df1898 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x72d8>
100dee15c:     	ldrh	w10, [x27, #0x14]
100dee160:     	cmp	w11, w10
100dee164:     	ldr	x2, [sp, #0x328]
100dee168:     	b.ls	0x100df19d0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7410>
100dee16c:     	ldrh	w19, [x27, #0xc]
100dee170:     	ldr	x9, [x9, #0x10]
100dee174:     	add	x8, x9, x8, lsl #3
100dee178:     	str	x8, [sp, #0xf8]
100dee17c:     	ldr	x5, [x8]
100dee180:     	add	x8, x9, x10, lsl #3
100dee184:     	str	x8, [sp, #0xc8]
100dee188:     	ldr	x6, [x8]
100dee18c:     	add	x0, sp, #0x420
100dee190:     	mov	x1, x20
100dee194:     	ldr	x3, [sp, #0x308]
100dee198:     	mov	w4, #0x10               ; =16
100dee19c:     	bl	0x100df6d00 <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm18runtime_activation16committed_valuesNtB9_11Interpreter21object_protocol_value>
100dee1a0:     	b	0x100def480 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4ec0>
100dee1a4:     	ldp	w5, w6, [x27, #0xc]
100dee1a8:     	ldr	x4, [sp, #0x360]
100dee1ac:     	add	x0, sp, #0x420
100dee1b0:     	mov	x1, x20
100dee1b4:     	mov	x2, x19
100dee1b8:     	ldr	x3, [sp, #0x328]
100dee1bc:     	bl	0x100ccbb00 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10global_opsNtB4_11Interpreter32run_load_global_or_undefined_reg>
100dee1c0:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100dee1c4:     	ldrh	w8, [x27, #0xc]
100dee1c8:     	ldr	x9, [sp, #0x358]
100dee1cc:     	ldr	w11, [x9, #0x18]
100dee1d0:     	cmp	w11, w8
100dee1d4:     	b.ls	0x100df15e8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7028>
100dee1d8:     	ldrh	w10, [x27, #0x10]
100dee1dc:     	ldr	x12, [sp, #0x1d8]
100dee1e0:     	str	x12, [sp, #0x1f8]
100dee1e4:     	cmp	w11, w10
100dee1e8:     	b.ls	0x100df15e8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7028>
100dee1ec:     	ldp	w6, w7, [x27, #0x14]
100dee1f0:     	ldr	x9, [x9, #0x10]
100dee1f4:     	add	x8, x9, x8, lsl #3
100dee1f8:     	str	x8, [sp, #0x1f8]
100dee1fc:     	ldr	x8, [x8]
100dee200:     	add	x9, x9, x10, lsl #3
100dee204:     	str	x9, [sp, #0x1d8]
100dee208:     	ldr	x5, [x9]
100dee20c:     	ldr	w4, [sp, #0x36c]
100dee210:     	ldr	x2, [x19]
100dee214:     	ldr	w3, [x19, #0x10]
100dee218:     	str	x8, [sp]
100dee21c:     	add	x0, sp, #0x420
100dee220:     	mov	x1, x20
100dee224:     	bl	0x100cd6150 <__RNvMNtCsgTc3P8JF8aG_8otter_vm11context_opsNtB4_11Interpreter21store_var_scope_value>
100dee228:     	b	0x100deeacc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x450c>
100dee22c:     	ldp	w4, w5, [x27, #0xc]
100dee230:     	ldr	x3, [sp, #0x360]
100dee234:     	add	x0, sp, #0x420
100dee238:     	mov	x1, x20
100dee23c:     	ldr	x2, [sp, #0x328]
100dee240:     	bl	0x100cfa808 <__RNvMNtCsgTc3P8JF8aG_8otter_vm14allocation_opsNtB4_11Interpreter19run_array_push_regs>
100dee244:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100dee248:     	ldp	w5, w6, [x27, #0xc]
100dee24c:     	ldr	x4, [sp, #0x360]
100dee250:     	add	x0, sp, #0x420
100dee254:     	mov	x1, x20
100dee258:     	mov	x2, x19
100dee25c:     	ldr	x3, [sp, #0x328]
100dee260:     	bl	0x100cf1600 <__RNvMNtCsgTc3P8JF8aG_8otter_vm12iterator_opsNtB4_11Interpreter27run_get_async_iterator_regs>
100dee264:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100dee268:     	ldr	w8, [x27, #0x14]
100dee26c:     	ldr	x9, [x19]
100dee270:     	ldr	x9, [x9, #0x30]
100dee274:     	ldr	x10, [x9, #0x18]
100dee278:     	cmp	x10, x8
100dee27c:     	b.ls	0x100df13a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6de8>
100dee280:     	ldr	x9, [x9, #0x10]
100dee284:     	add	x11, x9, x8, lsl #5
100dee288:     	ldr	x8, [x11]
100dee28c:     	cmn	x8, #0x1
100dee290:     	b.eq	0x100df13a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6de8>
100dee294:     	ldr	w5, [x27, #0xc]
100dee298:     	ldrh	w8, [x27, #0x10]
100dee29c:     	ldp	x9, x10, [x11, #0x8]
100dee2a0:     	ldr	wzr, [x11, #0x18]
100dee2a4:     	ldr	x11, [sp, #0x358]
100dee2a8:     	ldr	w12, [x11, #0x18]
100dee2ac:     	cmp	w12, w8
100dee2b0:     	ldr	x3, [sp, #0x328]
100dee2b4:     	b.ls	0x100df1a40 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7480>
100dee2b8:     	ldr	x11, [x11, #0x10]
100dee2bc:     	add	x8, x11, x8, lsl #3
100dee2c0:     	str	x8, [sp, #0x80]
100dee2c4:     	ldr	x6, [x8]
100dee2c8:     	mov	w8, #0x1                ; =1
100dee2cc:     	str	w8, [sp, #0x420]
100dee2d0:     	str	x9, [sp, #0x428]
100dee2d4:     	str	x10, [sp, #0x430]
100dee2d8:     	ldr	x4, [sp, #0x360]
100dee2dc:     	sub	x0, x29, #0xa8
100dee2e0:     	add	x7, sp, #0x420
100dee2e4:     	mov	x1, x20
100dee2e8:     	mov	x2, x19
100dee2ec:     	bl	0x100d0f2c0 <__RNvMNtCsgTc3P8JF8aG_8otter_vm17property_dispatchNtB4_11Interpreter23run_load_super_property>
100dee2f0:     	b	0x100def87c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x52bc>
100dee2f4:     	str	x25, [sp, #0x420]
100dee2f8:     	str	x27, [sp, #0x428]
100dee2fc:     	add	x8, sp, #0x358
100dee300:     	str	x8, [sp, #0x430]
100dee304:     	ldr	x8, [sp, #0x340]
100dee308:     	cbz	x8, 0x100df0150 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5b90>
100dee30c:     	add	x0, sp, #0x420
100dee310:     	bl	0x100caa868 <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickssi_0Bb_>
100dee314:     	mov	x23, x0
100dee318:     	mov	x0, x25
100dee31c:     	mov	x1, x27
100dee320:     	mov	w2, #0x1                ; =1
100dee324:     	bl	0x100db4240 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock7operand>
100dee328:     	tst	x0, #0xffff
100dee32c:     	ldr	x1, [sp, #0x328]
100dee330:     	b.ne	0x100defd84 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x57c4>
100dee334:     	ldr	x8, [sp, #0x358]
100dee338:     	ldr	w9, [x8, #0x18]
100dee33c:     	cmp	w9, w0, lsr #16
100dee340:     	b.ls	0x100defd84 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x57c4>
100dee344:     	ldr	x8, [x8, #0x10]
100dee348:     	cbz	x8, 0x100df0b74 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x65b4>
100dee34c:     	lsr	w9, w0, #16
100dee350:     	ldr	x0, [x8, x9, lsl #3]
100dee354:     	bl	0x100caa784 <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickssh_0Bb_>
100dee358:     	mov	x24, x0
100dee35c:     	mov	x26, x1
100dee360:     	str	xzr, [sp, #0x2a0]
100dee364:     	b	0x100df0158 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5b98>
100dee368:     	add	x0, sp, #0x420
100dee36c:     	mov	x1, x20
100dee370:     	mov	x2, x19
100dee374:     	ldr	x3, [sp, #0x328]
100dee378:     	mov	x4, x25
100dee37c:     	mov	x5, x27
100dee380:     	bl	0x100c06260 <__RINvMNtCsgTc3P8JF8aG_8otter_vm15static_call_opsNtB5_11Interpreter33run_copy_data_properties_operandsNtNtB5_10executable11OperandViewEB5_>
100dee384:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100dee388:     	ldp	w6, w7, [x27, #0xc]
100dee38c:     	ldr	x5, [sp, #0x360]
100dee390:     	ldr	x2, [x19]
100dee394:     	ldr	w3, [x19, #0x10]
100dee398:     	add	x0, sp, #0x420
100dee39c:     	mov	x1, x20
100dee3a0:     	ldr	x4, [sp, #0x328]
100dee3a4:     	bl	0x100cce89c <__RNvMNtCsgTc3P8JF8aG_8otter_vm10module_opsNtB4_11Interpreter24run_import_namespace_reg>
100dee3a8:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100dee3ac:     	ldp	w6, w7, [x27, #0xc]
100dee3b0:     	ldr	x5, [sp, #0x360]
100dee3b4:     	ldr	x2, [x19]
100dee3b8:     	ldr	w3, [x19, #0x10]
100dee3bc:     	add	x0, sp, #0x420
100dee3c0:     	mov	x1, x20
100dee3c4:     	ldr	x4, [sp, #0x328]
100dee3c8:     	bl	0x100cd05d8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10module_opsNtB4_11Interpreter28run_import_meta_resolve_regs>
100dee3cc:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100dee3d0:     	ldp	w6, w7, [x27, #0xc]
100dee3d4:     	ldr	x5, [sp, #0x360]
100dee3d8:     	ldr	x2, [x19]
100dee3dc:     	ldr	w3, [x19, #0x10]
100dee3e0:     	add	x0, sp, #0x420
100dee3e4:     	mov	x1, x20
100dee3e8:     	ldr	x4, [sp, #0x328]
100dee3ec:     	bl	0x100d16a64 <__RNvMNtCsgTc3P8JF8aG_8otter_vm19jit_class_value_opsNtB4_11Interpreter24run_new_private_name_reg>
100dee3f0:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100dee3f4:     	ldp	w5, w6, [x27, #0xc]
100dee3f8:     	ldr	x4, [sp, #0x358]
100dee3fc:     	ldr	x2, [x19]
100dee400:     	ldr	w3, [x19, #0x10]
100dee404:     	add	x0, sp, #0x420
100dee408:     	mov	x1, x20
100dee40c:     	bl	0x100cfa9c0 <__RNvMNtCsgTc3P8JF8aG_8otter_vm14allocation_opsNtB4_11Interpreter19run_load_regexp_reg>
100dee410:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100dee414:     	ldrh	w8, [x27, #0x10]
100dee418:     	ldr	x9, [sp, #0x358]
100dee41c:     	ldr	w10, [x9, #0x18]
100dee420:     	cmp	w10, w8
100dee424:     	b.ls	0x100df18c0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7300>
100dee428:     	ldrh	w19, [x27, #0xc]
100dee42c:     	ldr	x9, [x9, #0x10]
100dee430:     	add	x8, x9, x8, lsl #3
100dee434:     	str	x8, [sp, #0x148]
100dee438:     	ldr	x5, [x8]
100dee43c:     	add	x0, sp, #0x420
100dee440:     	mov	x1, x20
100dee444:     	ldr	x2, [sp, #0x328]
100dee448:     	ldr	x3, [sp, #0x308]
100dee44c:     	mov	w4, #0x1                ; =1
100dee450:     	mov	w6, #0xa                ; =10
100dee454:     	mov	w7, #0xa                ; =10
100dee458:     	bl	0x100df66fc <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm18runtime_activation16committed_valuesNtB9_11Interpreter12scalar_value>
100dee45c:     	b	0x100def480 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4ec0>
100dee460:     	add	x0, sp, #0x420
100dee464:     	mov	x1, x20
100dee468:     	mov	x2, x19
100dee46c:     	ldr	x3, [sp, #0x328]
100dee470:     	mov	x4, x25
100dee474:     	mov	x5, x27
100dee478:     	bl	0x100c04898 <__RINvMNtCsgTc3P8JF8aG_8otter_vm15static_call_opsNtB5_11Interpreter24run_for_in_keys_operandsNtNtB5_10executable11OperandViewEB5_>
100dee47c:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100dee480:     	ldr	w3, [x27, #0xc]
100dee484:     	ldr	x19, [sp, #0x358]
100dee488:     	ldr	w8, [x19, #0x38]
100dee48c:     	ldr	x9, [x19, #0x40]
100dee490:     	cmp	x9, #0x0
100dee494:     	csel	x10, xzr, x8, eq
100dee498:     	ldrh	w11, [x19, #0x8]
100dee49c:     	ldr	x12, [x19, #0x10]
100dee4a0:     	ccmp	x8, #0x0, #0x4, ne
100dee4a4:     	mov	w8, #0x8                ; =8
100dee4a8:     	csel	x9, x8, x9, eq
100dee4ac:     	cmp	x12, #0x0
100dee4b0:     	csel	x13, xzr, x11, eq
100dee4b4:     	ccmp	x11, #0x0, #0x4, ne
100dee4b8:     	csel	x8, x8, x12, eq
100dee4bc:     	str	x19, [sp, #0x420]
100dee4c0:     	str	x8, [sp, #0x428]
100dee4c4:     	str	x13, [sp, #0x430]
100dee4c8:     	str	x9, [sp, #0x438]
100dee4cc:     	str	x10, [sp, #0x440]
100dee4d0:     	sub	x0, x29, #0xa8
100dee4d4:     	add	x2, sp, #0x420
100dee4d8:     	mov	x1, x20
100dee4dc:     	bl	0x100d2bc28 <__RNvMNtCsgTc3P8JF8aG_8otter_vm9frame_opsNtB4_11Interpreter15frame_load_this>
100dee4e0:     	b	0x100dee71c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x415c>
100dee4e4:     	ldr	w8, [x27, #0x10]
100dee4e8:     	ldr	x9, [x19]
100dee4ec:     	ldr	x9, [x9, #0x30]
100dee4f0:     	ldr	x10, [x9, #0x18]
100dee4f4:     	cmp	x10, x8
100dee4f8:     	b.ls	0x100df13a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6de8>
100dee4fc:     	ldr	x9, [x9, #0x10]
100dee500:     	add	x10, x9, x8, lsl #5
100dee504:     	ldr	x8, [x10]
100dee508:     	cmn	x8, #0x1
100dee50c:     	b.eq	0x100df13a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6de8>
100dee510:     	ldrh	w8, [x27, #0xc]
100dee514:     	ldrh	w9, [x27, #0x14]
100dee518:     	ldp	x19, x21, [x10, #0x8]
100dee51c:     	ldr	wzr, [x10, #0x18]
100dee520:     	ldr	x10, [sp, #0x358]
100dee524:     	ldr	w11, [x10, #0x18]
100dee528:     	cmp	w11, w8
100dee52c:     	b.ls	0x100df1a68 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x74a8>
100dee530:     	cmp	w11, w9
100dee534:     	b.ls	0x100df1b30 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7570>
100dee538:     	ldr	x11, [x10, #0x10]
100dee53c:     	add	x8, x11, x8, lsl #3
100dee540:     	str	x8, [sp, #0x60]
100dee544:     	ldr	x23, [x8]
100dee548:     	add	x8, x11, x9, lsl #3
100dee54c:     	str	x8, [sp, #0x50]
100dee550:     	ldr	x24, [x8]
100dee554:     	ldr	w1, [x10]
100dee558:     	ldr	x22, [sp, #0x308]
100dee55c:     	mov	x0, x22
100dee560:     	bl	0x100dbd988 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm17execution_contextNtB5_16ExecutionContext18function_is_strict>
100dee564:     	mov	w8, #0x1                ; =1
100dee568:     	str	w8, [sp, #0x420]
100dee56c:     	str	x19, [sp, #0x428]
100dee570:     	str	x21, [sp, #0x430]
100dee574:     	ldr	x4, [sp, #0x360]
100dee578:     	strb	w0, [sp]
100dee57c:     	sub	x0, x29, #0xa8
100dee580:     	add	x6, sp, #0x420
100dee584:     	mov	x1, x20
100dee588:     	mov	x2, x22
100dee58c:     	ldr	x3, [sp, #0x328]
100dee590:     	mov	x5, x23
100dee594:     	mov	x7, x24
100dee598:     	bl	0x100d0fa40 <__RNvMNtCsgTc3P8JF8aG_8otter_vm17property_dispatchNtB4_11Interpreter24run_store_super_property>
100dee59c:     	b	0x100def87c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x52bc>
100dee5a0:     	ldrh	w8, [x27, #0x10]
100dee5a4:     	ldr	x9, [sp, #0x358]
100dee5a8:     	ldr	w10, [x9, #0x18]
100dee5ac:     	cmp	w10, w8
100dee5b0:     	b.ls	0x100df1884 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x72c4>
100dee5b4:     	ldrh	w19, [x27, #0xc]
100dee5b8:     	ldr	x9, [x9, #0x10]
100dee5bc:     	ldp	w5, w6, [x27, #0x14]
100dee5c0:     	add	x8, x9, x8, lsl #3
100dee5c4:     	str	x8, [sp, #0x128]
100dee5c8:     	ldr	x4, [x8]
100dee5cc:     	ldr	w3, [sp, #0x36c]
100dee5d0:     	ldr	x8, [sp, #0x308]
100dee5d4:     	ldr	x1, [x8]
100dee5d8:     	ldr	w2, [x8, #0x10]
100dee5dc:     	add	x0, sp, #0x420
100dee5e0:     	bl	0x100cd6fd4 <__RNvMNtCsgTc3P8JF8aG_8otter_vm11context_opsNtB4_11Interpreter24delete_lookup_slot_value>
100dee5e4:     	b	0x100def3bc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4dfc>
100dee5e8:     	ldr	w3, [x27, #0x14]
100dee5ec:     	mov	w8, #0xffff             ; =65535
100dee5f0:     	bics	wzr, w8, w3
100dee5f4:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100dee5f8:     	ldrh	w8, [x27, #0xc]
100dee5fc:     	ldr	x9, [sp, #0x358]
100dee600:     	ldr	w11, [x9, #0x18]
100dee604:     	cmp	w11, w8
100dee608:     	ldr	x19, [sp, #0x328]
100dee60c:     	ldr	x21, [sp, #0x2c8]
100dee610:     	b.ls	0x100df198c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x73cc>
100dee614:     	ldrh	w10, [x27, #0x10]
100dee618:     	cmp	w11, w10
100dee61c:     	b.ls	0x100df1a54 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7494>
100dee620:     	ldr	x9, [x9, #0x10]
100dee624:     	add	x8, x9, x8, lsl #3
100dee628:     	str	x8, [sp, #0x90]
100dee62c:     	ldr	x23, [x8]
100dee630:     	lsr	w2, w3, #16
100dee634:     	add	x8, x9, x10, lsl #3
100dee638:     	str	x8, [sp, #0x78]
100dee63c:     	ldr	x1, [x8]
100dee640:     	add	x0, sp, #0x420
100dee644:     	bl	0x100cd6c7c <__RNvMNtCsgTc3P8JF8aG_8otter_vm11context_opsNtB4_11Interpreter23derived_this_slot_value>
100dee648:     	ldrb	w8, [sp, #0x420]
100dee64c:     	cmp	w8, #0xff
100dee650:     	b.ne	0x100df1494 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6ed4>
100dee654:     	ldr	x6, [sp, #0x428]
100dee658:     	ldr	x2, [x19, #0x180]
100dee65c:     	add	x0, sp, #0x420
100dee660:     	mov	x1, x20
100dee664:     	mov	x3, x21
100dee668:     	mov	x4, x23
100dee66c:     	mov	w5, #0x1                ; =1
100dee670:     	bl	0x100e4b284 <__RNvMs_NtNtCsgTc3P8JF8aG_8otter_vm6interp6framesNtB8_11Interpreter15pop_frame_above>
100dee674:     	b	0x100dee79c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x41dc>
100dee678:     	add	x0, sp, #0x420
100dee67c:     	mov	x1, x20
100dee680:     	ldr	x2, [sp, #0x328]
100dee684:     	mov	x3, x19
100dee688:     	mov	x4, x25
100dee68c:     	mov	x5, x27
100dee690:     	bl	0x100c00e6c <__RINvMNtCsgTc3P8JF8aG_8otter_vm12function_opsNtB5_11Interpreter19drive_bind_functionNtNtB5_10executable11OperandViewEB5_>
100dee694:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100dee698:     	ldr	w4, [x27, #0xc]
100dee69c:     	ldr	x3, [sp, #0x360]
100dee6a0:     	add	x0, sp, #0x420
100dee6a4:     	mov	x1, x20
100dee6a8:     	ldr	x2, [sp, #0x328]
100dee6ac:     	bl	0x100cfa71c <__RNvMNtCsgTc3P8JF8aG_8otter_vm14allocation_opsNtB4_11Interpreter18run_new_object_reg>
100dee6b0:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100dee6b4:     	ldp	w4, w5, [x27, #0xc]
100dee6b8:     	ldr	w6, [x27, #0x14]
100dee6bc:     	mov	x2, x19
100dee6c0:     	ldr	x19, [sp, #0x358]
100dee6c4:     	ldr	w8, [x19, #0x38]
100dee6c8:     	ldr	x9, [x19, #0x40]
100dee6cc:     	cmp	x9, #0x0
100dee6d0:     	csel	x10, xzr, x8, eq
100dee6d4:     	ldrh	w11, [x19, #0x8]
100dee6d8:     	ldr	x12, [x19, #0x10]
100dee6dc:     	ccmp	x8, #0x0, #0x4, ne
100dee6e0:     	mov	w8, #0x8                ; =8
100dee6e4:     	csel	x9, x8, x9, eq
100dee6e8:     	cmp	x12, #0x0
100dee6ec:     	csel	x13, xzr, x11, eq
100dee6f0:     	ccmp	x11, #0x0, #0x4, ne
100dee6f4:     	csel	x8, x8, x12, eq
100dee6f8:     	str	x19, [sp, #0x420]
100dee6fc:     	str	x8, [sp, #0x428]
100dee700:     	str	x13, [sp, #0x430]
100dee704:     	str	x9, [sp, #0x438]
100dee708:     	str	x10, [sp, #0x440]
100dee70c:     	sub	x0, x29, #0xa8
100dee710:     	add	x3, sp, #0x420
100dee714:     	mov	x1, x20
100dee718:     	bl	0x100cd5f7c <__RNvMNtCsgTc3P8JF8aG_8otter_vm11context_opsNtB4_11Interpreter20frame_create_context>
100dee71c:     	ldurb	w8, [x29, #-0xa8]
100dee720:     	cmp	w8, #0xff
100dee724:     	b.ne	0x100df1564 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6fa4>
100dee728:     	ldr	w8, [x19, #0x4]
100dee72c:     	cmn	w8, #0x1
100dee730:     	b.ne	0x100df05d0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6010>
100dee734:     	b	0x100df1e0c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x784c>
100dee738:     	ldr	w5, [x27, #0xc]
100dee73c:     	ldr	x3, [sp, #0x360]
100dee740:     	ldrh	w4, [x25, #0x120]
100dee744:     	add	x0, sp, #0x420
100dee748:     	mov	x1, x20
100dee74c:     	ldr	x2, [sp, #0x328]
100dee750:     	bl	0x100d2b808 <__RNvMNtCsgTc3P8JF8aG_8otter_vm9frame_opsNtB4_11Interpreter12collect_rest>
100dee754:     	b	0x100def700 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5140>
100dee758:     	ldr	w6, [x27, #0xc]
100dee75c:     	ldr	x5, [sp, #0x360]
100dee760:     	ldr	x2, [x19]
100dee764:     	ldr	w3, [x19, #0x10]
100dee768:     	add	x0, sp, #0x420
100dee76c:     	mov	x1, x20
100dee770:     	ldr	x4, [sp, #0x328]
100dee774:     	bl	0x100d1ada0 <__RNvMNtCsgTc3P8JF8aG_8otter_vm19jit_spread_call_opsNtB4_11Interpreter25run_collect_arguments_reg>
100dee778:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100dee77c:     	ldr	x8, [sp, #0x328]
100dee780:     	ldr	x2, [x8, #0x180]
100dee784:     	add	x0, sp, #0x420
100dee788:     	mov	x1, x20
100dee78c:     	ldr	x3, [sp, #0x2c8]
100dee790:     	mov	w4, #0xa                ; =10
100dee794:     	mov	x5, #0x0                ; =0
100dee798:     	bl	0x100e4b284 <__RNvMs_NtNtCsgTc3P8JF8aG_8otter_vm6interp6framesNtB8_11Interpreter15pop_frame_above>
100dee79c:     	ldrb	w9, [sp, #0x420]
100dee7a0:     	cmp	w9, #0xff
100dee7a4:     	b.ne	0x100df14bc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6efc>
100dee7a8:     	ldr	x8, [sp, #0x428]
100dee7ac:     	cbz	x8, 0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100dee7b0:     	b	0x100df14f0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6f30>
100dee7b4:     	ldp	w5, w6, [x27, #0xc]
100dee7b8:     	ldr	x4, [sp, #0x358]
100dee7bc:     	ldr	x2, [x19]
100dee7c0:     	ldr	w3, [x19, #0x10]
100dee7c4:     	add	x0, sp, #0x420
100dee7c8:     	mov	x1, x20
100dee7cc:     	bl	0x100cc5f68 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10global_opsNtB4_11Interpreter23run_init_global_lex_reg>
100dee7d0:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100dee7d4:     	ldrh	w8, [x27, #0x14]
100dee7d8:     	ldr	x9, [sp, #0x358]
100dee7dc:     	ldr	w10, [x9, #0x18]
100dee7e0:     	cmp	w10, w8
100dee7e4:     	b.ls	0x100df1794 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x71d4>
100dee7e8:     	ldrh	w21, [x27, #0xc]
100dee7ec:     	ldrh	w19, [x27, #0x10]
100dee7f0:     	ldr	x9, [x9, #0x10]
100dee7f4:     	add	x8, x9, x8, lsl #3
100dee7f8:     	str	x8, [sp, #0x160]
100dee7fc:     	ldr	x4, [x8]
100dee800:     	add	x0, sp, #0x420
100dee804:     	mov	x1, x20
100dee808:     	ldr	x2, [sp, #0x328]
100dee80c:     	ldr	x3, [sp, #0x308]
100dee810:     	bl	0x100cf07c4 <__RNvMNtCsgTc3P8JF8aG_8otter_vm12iterator_opsNtB4_11Interpreter26async_iterator_return_call>
100dee814:     	ldr	x8, [sp, #0x420]
100dee818:     	cmp	x8, #0x2
100dee81c:     	b.ne	0x100df0ec4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6904>
100dee820:     	ldr	x9, [sp, #0x428]
100dee824:     	ldr	x8, [sp, #0x358]
100dee828:     	cbz	x9, 0x100deffac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x59ec>
100dee82c:     	ldrh	w10, [x8, #0x8]
100dee830:     	ldr	x9, [x8, #0x10]
100dee834:     	cmp	w10, w21
100dee838:     	ccmp	x9, #0x0, #0x4, hi
100dee83c:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100dee840:     	ldr	x10, [sp, #0x430]
100dee844:     	str	x10, [x9, w21, uxtw #3]
100dee848:     	ldrh	w10, [x8, #0x8]
100dee84c:     	ldr	x9, [x8, #0x10]
100dee850:     	cmp	w10, w19
100dee854:     	ccmp	x9, #0x0, #0x4, hi
100dee858:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100dee85c:     	mov	w10, #0x7               ; =7
100dee860:     	str	x10, [x9, x19, lsl #3]
100dee864:     	b	0x100def710 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5150>
100dee868:     	ldr	w8, [x27, #0x14]
100dee86c:     	ldr	x9, [x19]
100dee870:     	ldr	x9, [x9, #0x30]
100dee874:     	ldr	x10, [x9, #0x18]
100dee878:     	cmp	x10, x8
100dee87c:     	b.ls	0x100df13a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6de8>
100dee880:     	ldr	x9, [x9, #0x10]
100dee884:     	add	x8, x9, x8, lsl #5
100dee888:     	ldr	x9, [x8]
100dee88c:     	cmn	x9, #0x1
100dee890:     	b.eq	0x100df13a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6de8>
100dee894:     	ldp	w23, w24, [x27, #0xc]
100dee898:     	ldp	x26, x19, [x8, #0x8]
100dee89c:     	ldr	w8, [x8, #0x18]
100dee8a0:     	ldr	w9, [x27]
100dee8a4:     	ldr	x10, [x25, #0xe8]
100dee8a8:     	cmp	x10, x9
100dee8ac:     	ldr	x2, [sp, #0x328]
100dee8b0:     	b.ls	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100dee8b4:     	ldr	x10, [x25, #0xe0]
100dee8b8:     	mov	w11, #0x18              ; =24
100dee8bc:     	umaddl	x9, w9, w11, x10
100dee8c0:     	ldrh	w10, [x9]
100dee8c4:     	sub	w10, w10, #0x1
100dee8c8:     	cmp	w10, #0x1
100dee8cc:     	b.hi	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100dee8d0:     	ldr	x9, [x9, #0x8]
100dee8d4:     	ldrb	w10, [x9, #0xb0]
100dee8d8:     	cbnz	w10, 0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100dee8dc:     	stp	x26, x19, [x29, #-0xa8]
100dee8e0:     	stur	x8, [x29, #-0x98]
100dee8e4:     	str	x9, [sp]
100dee8e8:     	add	x0, sp, #0x420
100dee8ec:     	sub	x6, x29, #0xa8
100dee8f0:     	add	x7, x25, #0xd0
100dee8f4:     	mov	x1, x20
100dee8f8:     	ldr	x3, [sp, #0x308]
100dee8fc:     	mov	x4, x23
100dee900:     	mov	x5, x24
100dee904:     	bl	0x100d37e40 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm17property_dispatch7driversNtB6_11Interpreter19drive_load_property>
100dee908:     	ldr	x8, [sp, #0x420]
100dee90c:     	cmp	x8, #0x2
100dee910:     	b.ne	0x100df0ec4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6904>
100dee914:     	ldrb	w8, [sp, #0x428]
100dee918:     	tbnz	w8, #0x0, 0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100dee91c:     	ldr	x4, [sp, #0x360]
100dee920:     	str	x19, [sp]
100dee924:     	add	x0, sp, #0x420
100dee928:     	mov	x1, x20
100dee92c:     	ldr	x2, [sp, #0x308]
100dee930:     	ldr	x3, [sp, #0x328]
100dee934:     	mov	x5, x23
100dee938:     	mov	x6, x24
100dee93c:     	mov	x7, x26
100dee940:     	bl	0x100d314b0 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm17property_dispatch10propertiesNtB6_11Interpreter21run_load_property_reg>
100dee944:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100dee948:     	ldr	x8, [sp, #0x340]
100dee94c:     	cbz	x8, 0x100dee98c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x43cc>
100dee950:     	mov	x0, x25
100dee954:     	mov	x1, x27
100dee958:     	mov	w2, #0x1                ; =1
100dee95c:     	bl	0x100db4320 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock8register>
100dee960:     	tbz	w0, #0x0, 0x100dee98c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x43cc>
100dee964:     	ldr	x8, [sp, #0x358]
100dee968:     	ldr	w9, [x8, #0x18]
100dee96c:     	cmp	w9, w1, uxth
100dee970:     	b.ls	0x100dee98c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x43cc>
100dee974:     	ldr	x8, [x8, #0x10]
100dee978:     	and	x9, x1, #0xffff
100dee97c:     	ldr	x2, [x8, x9, lsl #3]
100dee980:     	ldr	w1, [x27]
100dee984:     	mov	x0, x25
100dee988:     	bl	0x100d0bbc0 <__RNvMNtCsgTc3P8JF8aG_8otter_vm16element_feedbackNtB4_11Interpreter30record_element_family_feedback>
100dee98c:     	add	x0, sp, #0x420
100dee990:     	mov	x1, x20
100dee994:     	ldr	x2, [sp, #0x328]
100dee998:     	ldr	x3, [sp, #0x308]
100dee99c:     	mov	x4, x25
100dee9a0:     	mov	x5, x27
100dee9a4:     	bl	0x100c08c14 <__RINvMNtNtCsgTc3P8JF8aG_8otter_vm17property_dispatch7driversNtB7_11Interpreter18drive_load_elementNtNtB7_10executable11OperandViewEB7_>
100dee9a8:     	ldr	x8, [sp, #0x420]
100dee9ac:     	cmp	x8, #0x2
100dee9b0:     	b.ne	0x100df0ec4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6904>
100dee9b4:     	ldrb	w8, [sp, #0x428]
100dee9b8:     	tbnz	w8, #0x0, 0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100dee9bc:     	ldp	w5, w6, [x27, #0xc]
100dee9c0:     	ldr	w7, [x27, #0x14]
100dee9c4:     	ldr	x4, [sp, #0x360]
100dee9c8:     	add	x0, sp, #0x420
100dee9cc:     	mov	x1, x20
100dee9d0:     	ldr	x2, [sp, #0x308]
100dee9d4:     	ldr	x3, [sp, #0x328]
100dee9d8:     	bl	0x100d3cb48 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm17property_dispatch8elementsNtB6_11Interpreter21run_load_element_regs>
100dee9dc:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100dee9e0:     	ldrb	w8, [x27, #0x1d]
100dee9e4:     	cmp	w8, #0x4
100dee9e8:     	ldr	x19, [sp, #0x328]
100dee9ec:     	b.lo	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100dee9f0:     	b.ne	0x100defe80 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x58c0>
100dee9f4:     	add	x8, x27, #0x18
100dee9f8:     	b	0x100defe9c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x58dc>
100dee9fc:     	ldr	w5, [x27, #0xc]
100deea00:     	ldrb	w8, [x27, #0x1d]
100deea04:     	cmp	w8, #0x2
100deea08:     	b.lo	0x100df03fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5e3c>
100deea0c:     	cmp	w8, #0x5
100deea10:     	b.hs	0x100df03dc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5e1c>
100deea14:     	ldr	w9, [x27, #0x10]
100deea18:     	b	0x100df03f4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5e34>
100deea1c:     	ldp	w3, w4, [x27, #0xc]
100deea20:     	ldr	w5, [x27, #0x14]
100deea24:     	ldr	x2, [sp, #0x358]
100deea28:     	str	x26, [sp]
100deea2c:     	add	x0, sp, #0x420
100deea30:     	mov	x1, x20
100deea34:     	mov	w6, #0x0                ; =0
100deea38:     	mov	x7, x28
100deea3c:     	bl	0x100e35788 <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB6_11Interpreter13run_equal_imm>
100deea40:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100deea44:     	ldrh	w8, [x27, #0x10]
100deea48:     	ldr	x9, [sp, #0x358]
100deea4c:     	ldr	w10, [x9, #0x18]
100deea50:     	cmp	w10, w8
100deea54:     	b.ls	0x100df1848 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7288>
100deea58:     	ldrh	w19, [x27, #0xc]
100deea5c:     	ldr	x9, [x9, #0x10]
100deea60:     	add	x8, x9, x8, lsl #3
100deea64:     	str	x8, [sp, #0x158]
100deea68:     	ldr	x5, [x8]
100deea6c:     	add	x0, sp, #0x420
100deea70:     	mov	x1, x20
100deea74:     	ldr	x2, [sp, #0x328]
100deea78:     	ldr	x3, [sp, #0x308]
100deea7c:     	mov	w4, #0x12               ; =18
100deea80:     	mov	w6, #0xa                ; =10
100deea84:     	bl	0x100df6d00 <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm18runtime_activation16committed_valuesNtB9_11Interpreter21object_protocol_value>
100deea88:     	b	0x100def480 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4ec0>
100deea8c:     	ldrh	w8, [x27, #0xc]
100deea90:     	ldr	x9, [sp, #0x358]
100deea94:     	ldr	w10, [x9, #0x18]
100deea98:     	cmp	w10, w8
100deea9c:     	b.ls	0x100df176c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x71ac>
100deeaa0:     	ldr	x9, [x9, #0x10]
100deeaa4:     	ldp	w6, w7, [x27, #0x10]
100deeaa8:     	add	x8, x9, x8, lsl #3
100deeaac:     	str	x8, [sp, #0x138]
100deeab0:     	ldr	x5, [x8]
100deeab4:     	ldr	w4, [sp, #0x36c]
100deeab8:     	ldr	x2, [x19]
100deeabc:     	ldr	w3, [x19, #0x10]
100deeac0:     	add	x0, sp, #0x420
100deeac4:     	mov	x1, x20
100deeac8:     	bl	0x100cd67d0 <__RNvMNtCsgTc3P8JF8aG_8otter_vm11context_opsNtB4_11Interpreter22declare_eval_var_value>
100deeacc:     	ldrb	w8, [sp, #0x420]
100deead0:     	cmp	w8, #0xff
100deead4:     	b.eq	0x100def70c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x514c>
100deead8:     	b	0x100df0e20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6860>
100deeadc:     	ldrb	w9, [x27, #0x1d]
100deeae0:     	cmp	w9, #0x3
100deeae4:     	b.lo	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100deeae8:     	ldr	w19, [x27, #0xc]
100deeaec:     	ldrh	w8, [x27, #0x10]
100deeaf0:     	cmp	w9, #0x5
100deeaf4:     	b.hs	0x100defed8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5918>
100deeaf8:     	add	x9, x27, #0x14
100deeafc:     	b	0x100defef0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5930>
100deeb00:     	ldr	w5, [x27, #0xc]
100deeb04:     	ldrb	w8, [x27, #0x1d]
100deeb08:     	cmp	w8, #0x3
100deeb0c:     	b.lo	0x100df0444 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5e84>
100deeb10:     	cmp	w8, #0x5
100deeb14:     	b.hs	0x100df0424 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5e64>
100deeb18:     	ldr	w9, [x27, #0x14]
100deeb1c:     	b	0x100df043c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5e7c>
100deeb20:     	ldr	w5, [x27, #0xc]
100deeb24:     	ldrb	w8, [x27, #0x1d]
100deeb28:     	cmp	w8, #0x2
100deeb2c:     	b.lo	0x100df0490 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5ed0>
100deeb30:     	cmp	w8, #0x5
100deeb34:     	b.hs	0x100df0470 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5eb0>
100deeb38:     	ldr	w9, [x27, #0x10]
100deeb3c:     	b	0x100df0488 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5ec8>
100deeb40:     	ldrh	w8, [x27, #0xc]
100deeb44:     	ldr	x9, [sp, #0x358]
100deeb48:     	ldr	w11, [x9, #0x18]
100deeb4c:     	cmp	w11, w8
100deeb50:     	b.ls	0x100df15d4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7014>
100deeb54:     	ldrh	w10, [x27, #0x10]
100deeb58:     	ldr	x12, [sp, #0x1e0]
100deeb5c:     	str	x12, [sp, #0x200]
100deeb60:     	cmp	w11, w10
100deeb64:     	b.ls	0x100df15d4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7014>
100deeb68:     	ldp	w6, w7, [x27, #0x14]
100deeb6c:     	ldr	x9, [x9, #0x10]
100deeb70:     	add	x8, x9, x8, lsl #3
100deeb74:     	str	x8, [sp, #0x200]
100deeb78:     	ldr	x8, [x8]
100deeb7c:     	add	x9, x9, x10, lsl #3
100deeb80:     	str	x9, [sp, #0x1e0]
100deeb84:     	ldr	x5, [x9]
100deeb88:     	ldr	w4, [sp, #0x36c]
100deeb8c:     	str	x8, [sp]
100deeb90:     	add	x0, sp, #0x420
100deeb94:     	mov	x1, x20
100deeb98:     	mov	x2, x19
100deeb9c:     	ldr	x3, [sp, #0x328]
100deeba0:     	bl	0x100cd4df4 <__RNvMNtCsgTc3P8JF8aG_8otter_vm11context_opsNtB4_11Interpreter15store_ref_value>
100deeba4:     	b	0x100def300 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4d40>
100deeba8:     	add	x0, sp, #0x420
100deebac:     	mov	x1, x20
100deebb0:     	mov	x2, x19
100deebb4:     	ldr	x3, [sp, #0x328]
100deebb8:     	mov	x4, x25
100deebbc:     	mov	x5, x27
100deebc0:     	bl	0x100c38cec <__RINvMs_NtCsgTc3P8JF8aG_8otter_vm8eval_opsNtB7_11Interpreter17run_eval_operandsNtNtB7_10executable11OperandViewEB7_>
100deebc4:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100deebc8:     	ldp	w6, w7, [x27, #0xc]
100deebcc:     	ldr	x5, [sp, #0x360]
100deebd0:     	ldr	x2, [x19]
100deebd4:     	ldr	w3, [x19, #0x10]
100deebd8:     	add	x0, sp, #0x420
100deebdc:     	mov	x1, x20
100deebe0:     	ldr	x4, [sp, #0x328]
100deebe4:     	bl	0x100cd428c <__RNvMNtCsgTc3P8JF8aG_8otter_vm10module_opsNtB4_11Interpreter33run_import_namespace_deferred_reg>
100deebe8:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100deebec:     	add	x0, sp, #0x420
100deebf0:     	mov	x1, x20
100deebf4:     	ldr	x2, [sp, #0x328]
100deebf8:     	mov	x3, x25
100deebfc:     	mov	x4, x27
100deec00:     	bl	0x100c068c8 <__RINvMNtCsgTc3P8JF8aG_8otter_vm15static_call_opsNtB5_11Interpreter44run_shared_array_buffer_static_call_operandsNtNtB5_10executable11OperandViewEB5_>
100deec04:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100deec08:     	ldp	w5, w6, [x27, #0xc]
100deec0c:     	ldr	x4, [sp, #0x360]
100deec10:     	add	x0, sp, #0x420
100deec14:     	mov	x1, x20
100deec18:     	mov	x2, x19
100deec1c:     	ldr	x3, [sp, #0x328]
100deec20:     	bl	0x100ce5540 <__RNvMNtCsgTc3P8JF8aG_8otter_vm12iterator_opsNtB4_11Interpreter13spread_append>
100deec24:     	b	0x100def300 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4d40>
100deec28:     	add	x0, sp, #0x420
100deec2c:     	mov	x1, x20
100deec30:     	mov	x2, x19
100deec34:     	ldr	x3, [sp, #0x328]
100deec38:     	mov	x4, x25
100deec3c:     	mov	x5, x27
100deec40:     	bl	0x100c05e64 <__RINvMNtCsgTc3P8JF8aG_8otter_vm15static_call_opsNtB5_11Interpreter32run_define_own_property_operandsNtNtB5_10executable11OperandViewEB5_>
100deec44:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100deec48:     	ldrh	w8, [x27, #0x10]
100deec4c:     	ldr	x9, [sp, #0x358]
100deec50:     	ldr	w10, [x9, #0x18]
100deec54:     	cmp	w10, w8
100deec58:     	b.ls	0x100df185c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x729c>
100deec5c:     	ldrh	w19, [x27, #0xc]
100deec60:     	ldr	x9, [x9, #0x10]
100deec64:     	add	x8, x9, x8, lsl #3
100deec68:     	str	x8, [sp, #0x168]
100deec6c:     	ldr	x5, [x8]
100deec70:     	add	x0, sp, #0x420
100deec74:     	mov	x1, x20
100deec78:     	ldr	x2, [sp, #0x328]
100deec7c:     	ldr	x3, [sp, #0x308]
100deec80:     	mov	w4, #0x5                ; =5
100deec84:     	mov	w6, #0xa                ; =10
100deec88:     	mov	w7, #0xa                ; =10
100deec8c:     	bl	0x100df66fc <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm18runtime_activation16committed_valuesNtB9_11Interpreter12scalar_value>
100deec90:     	b	0x100def480 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4ec0>
100deec94:     	add	x0, sp, #0x420
100deec98:     	mov	x1, x20
100deec9c:     	mov	x2, x19
100deeca0:     	ldr	x3, [sp, #0x328]
100deeca4:     	mov	x4, x25
100deeca8:     	mov	x5, x27
100deecac:     	bl	0x100c05c74 <__RINvMNtCsgTc3P8JF8aG_8otter_vm15static_call_opsNtB5_11Interpreter26run_star_reexport_operandsNtNtB5_10executable11OperandViewEB5_>
100deecb0:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100deecb4:     	ldp	w4, w5, [x27, #0xc]
100deecb8:     	ldr	x3, [sp, #0x358]
100deecbc:     	add	x0, sp, #0x420
100deecc0:     	mov	x1, x20
100deecc4:     	mov	x2, x19
100deecc8:     	bl	0x100cdbeec <__RNvMNtCsgTc3P8JF8aG_8otter_vm12function_opsNtB4_11Interpreter21run_make_function_reg>
100deeccc:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100deecd0:     	add	x0, sp, #0x420
100deecd4:     	mov	x1, x20
100deecd8:     	ldr	x2, [sp, #0x328]
100deecdc:     	mov	x3, x19
100deece0:     	mov	x4, x25
100deece4:     	mov	x5, x27
100deece8:     	bl	0x100c0abdc <__RINvMNtNtCsgTc3P8JF8aG_8otter_vm17property_dispatch7driversNtB7_11Interpreter26drive_delete_element_proxyNtNtB7_10executable11OperandViewEB7_>
100deecec:     	ldr	x8, [sp, #0x420]
100deecf0:     	cmp	x8, #0x2
100deecf4:     	b.ne	0x100df0ec4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6904>
100deecf8:     	ldrb	w8, [sp, #0x428]
100deecfc:     	tbnz	w8, #0x0, 0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100deed00:     	ldr	w24, [x27, #0xc]
100deed04:     	ldrh	w23, [x27, #0x10]
100deed08:     	ldrh	w26, [x27, #0x14]
100deed0c:     	ldr	x19, [sp, #0x358]
100deed10:     	ldr	w1, [x19]
100deed14:     	ldr	x0, [sp, #0x308]
100deed18:     	bl	0x100dbd988 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm17execution_contextNtB5_16ExecutionContext18function_is_strict>
100deed1c:     	ldr	w8, [x19, #0x18]
100deed20:     	cmp	w8, w23
100deed24:     	b.ls	0x100df1c28 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7668>
100deed28:     	mov	x21, #0x2               ; =2
100deed2c:     	movk	x21, #0xfffe, lsl #48
100deed30:     	ldr	x8, [x19, #0x10]
100deed34:     	add	x9, x8, x23, lsl #3
100deed38:     	ldr	x27, [x9]
100deed3c:     	and	x8, x27, x21
100deed40:     	cmp	x27, #0x0
100deed44:     	ccmp	x8, #0x0, #0x0, ne
100deed48:     	ccmp	w27, #0x0, #0x4, eq
100deed4c:     	str	x9, [sp, #0x30]
100deed50:     	b.eq	0x100df0c10 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6650>
100deed54:     	mov	x19, x0
100deed58:     	adrp	x8, 0x1034f4000 <_global_added_lock+0x68>
100deed5c:     	add	x8, x8, #0xef8
100deed60:     	mov	w22, #-0x3              ; =-3
100deed64:     	ldr	x8, [x8]
100deed68:     	and	x9, x27, x22
100deed6c:     	ldrb	w8, [x8, x9]
100deed70:     	cmp	w8, #0x11
100deed74:     	b.ne	0x100df0c0c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x664c>
100deed78:     	mov	x0, x27
100deed7c:     	bl	0x100ca4ec8 <__RNCNvMNtCsgTc3P8JF8aG_8otter_vm14jit_delete_opsNtB6_11Interpreter21jit_runtime_delete_op0B6_>
100deed80:     	cbz	w0, 0x100df0c0c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x664c>
100deed84:     	ldr	x8, [sp, #0x358]
100deed88:     	ldr	w9, [x8, #0x18]
100deed8c:     	cmp	w9, w26
100deed90:     	b.ls	0x100df1df8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7838>
100deed94:     	ldr	x8, [x8, #0x10]
100deed98:     	add	x8, x8, x26, lsl #3
100deed9c:     	str	x8, [sp, #0x20]
100deeda0:     	ldr	x28, [x8]
100deeda4:     	add	x0, sp, #0x420
100deeda8:     	mov	x1, x28
100deedac:     	bl	0x100d259f4 <__RNvMNtCsgTc3P8JF8aG_8otter_vm5valueNtB2_5Value9as_symbol>
100deedb0:     	ldr	w8, [sp, #0x420]
100deedb4:     	cmp	w8, #0x2
100deedb8:     	b.ne	0x100df0bb0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x65f0>
100deedbc:     	and	x8, x28, x21
100deedc0:     	cmp	x28, #0x0
100deedc4:     	ccmp	x8, #0x0, #0x0, ne
100deedc8:     	b.ne	0x100df0be4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6624>
100deedcc:     	cbz	w28, 0x100df0be4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6624>
100deedd0:     	adrp	x8, 0x1034f4000 <_global_added_lock+0x68>
100deedd4:     	add	x8, x8, #0xef8
100deedd8:     	ldr	x8, [x8]
100deeddc:     	and	x9, x28, x22
100deede0:     	ldrb	w8, [x8, x9]
100deede4:     	cmp	w8, #0x20
100deede8:     	b.ne	0x100df0be4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6624>
100deedec:     	adrp	x8, 0x1034f4000 <_global_added_lock+0x68>
100deedf0:     	add	x8, x8, #0xef8
100deedf4:     	ldr	xzr, [x8]
100deedf8:     	add	x0, x20, #0x430
100deedfc:     	mov	x1, x28
100deee00:     	bl	0x100ca4fe4 <__RNCNvMNtCsgTc3P8JF8aG_8otter_vm14jit_delete_opsNtB6_11Interpreter21jit_runtime_delete_ops0_0B6_>
100deee04:     	eor	w5, w0, #0x1
100deee08:     	b	0x100df0be8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6628>
100deee0c:     	ldp	w5, w6, [x27, #0xc]
100deee10:     	ldr	w7, [x27, #0x14]
100deee14:     	ldr	x4, [sp, #0x360]
100deee18:     	add	x0, sp, #0x420
100deee1c:     	mov	x1, x20
100deee20:     	mov	x2, x19
100deee24:     	ldr	x3, [sp, #0x328]
100deee28:     	bl	0x100d2a07c <__RNvMNtCsgTc3P8JF8aG_8otter_vm9class_opsNtB4_11Interpreter25run_set_function_name_reg>
100deee2c:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100deee30:     	add	x0, sp, #0x420
100deee34:     	mov	x1, x20
100deee38:     	ldr	x2, [sp, #0x328]
100deee3c:     	mov	x3, x19
100deee40:     	mov	x4, x25
100deee44:     	mov	x5, x27
100deee48:     	bl	0x100c013f0 <__RINvMNtCsgTc3P8JF8aG_8otter_vm12iterator_opsNtB5_11Interpreter18drive_get_iteratorNtNtB5_10executable11OperandViewEB5_>
100deee4c:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100deee50:     	ldrh	w8, [x27, #0x10]
100deee54:     	ldr	x9, [sp, #0x358]
100deee58:     	ldr	w11, [x9, #0x18]
100deee5c:     	cmp	w11, w8
100deee60:     	b.ls	0x100df171c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x715c>
100deee64:     	ldrh	w10, [x27, #0x14]
100deee68:     	cmp	w11, w10
100deee6c:     	ldr	x2, [sp, #0x328]
100deee70:     	b.ls	0x100df19e4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7424>
100deee74:     	ldrh	w19, [x27, #0xc]
100deee78:     	ldr	x9, [x9, #0x10]
100deee7c:     	add	x8, x9, x8, lsl #3
100deee80:     	str	x8, [sp, #0x108]
100deee84:     	ldr	x5, [x8]
100deee88:     	add	x8, x9, x10, lsl #3
100deee8c:     	str	x8, [sp, #0xd0]
100deee90:     	ldr	x6, [x8]
100deee94:     	add	x0, sp, #0x420
100deee98:     	mov	x1, x20
100deee9c:     	ldr	x3, [sp, #0x308]
100deeea0:     	mov	w4, #0x11               ; =17
100deeea4:     	bl	0x100df6d00 <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm18runtime_activation16committed_valuesNtB9_11Interpreter21object_protocol_value>
100deeea8:     	b	0x100def480 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4ec0>
100deeeac:     	ldrh	w8, [x27, #0x10]
100deeeb0:     	ldr	x9, [sp, #0x358]
100deeeb4:     	ldr	w10, [x9, #0x18]
100deeeb8:     	cmp	w10, w8
100deeebc:     	b.ls	0x100df1870 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x72b0>
100deeec0:     	ldrh	w19, [x27, #0xc]
100deeec4:     	ldr	x9, [x9, #0x10]
100deeec8:     	add	x21, x9, x8, lsl #3
100deeecc:     	ldr	x23, [x21]
100deeed0:     	mov	x0, x23
100deeed4:     	bl	0x100d24a18 <__RNvMNtCsgTc3P8JF8aG_8otter_vm5valueNtB2_5Value12is_primitive>
100deeed8:     	ldr	x2, [sp, #0x328]
100deeedc:     	str	x21, [sp, #0x170]
100deeee0:     	cbz	w0, 0x100deff90 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x59d0>
100deeee4:     	ldr	x8, [sp, #0x358]
100deeee8:     	ldrh	w9, [x8, #0x8]
100deeeec:     	ldr	x8, [x8, #0x10]
100deeef0:     	cmp	w9, w19
100deeef4:     	ccmp	x8, #0x0, #0x4, hi
100deeef8:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100deeefc:     	str	x23, [x8, x19, lsl #3]
100deef00:     	b	0x100def70c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x514c>
100deef04:     	add	x0, sp, #0x420
100deef08:     	mov	x1, x20
100deef0c:     	ldr	x2, [sp, #0x328]
100deef10:     	mov	x3, x19
100deef14:     	mov	x4, x25
100deef18:     	mov	x5, x27
100deef1c:     	bl	0x100c0b0c8 <__RINvMNtNtCsgTc3P8JF8aG_8otter_vm17property_dispatch7driversNtB7_11Interpreter27drive_delete_property_proxyNtNtB7_10executable11OperandViewEB7_>
100deef20:     	ldr	x8, [sp, #0x420]
100deef24:     	cmp	x8, #0x2
100deef28:     	b.ne	0x100df0ec4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6904>
100deef2c:     	ldrb	w8, [sp, #0x428]
100deef30:     	tbnz	w8, #0x0, 0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100deef34:     	ldr	w8, [x27, #0x14]
100deef38:     	ldr	x9, [sp, #0x308]
100deef3c:     	ldr	x9, [x9]
100deef40:     	ldr	x9, [x9, #0x30]
100deef44:     	ldr	x10, [x9, #0x18]
100deef48:     	cmp	x10, x8
100deef4c:     	b.ls	0x100df13a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6de8>
100deef50:     	ldr	x9, [x9, #0x10]
100deef54:     	add	x8, x9, x8, lsl #5
100deef58:     	ldr	x9, [x8]
100deef5c:     	cmn	x9, #0x1
100deef60:     	b.eq	0x100df13a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6de8>
100deef64:     	ldr	w24, [x27, #0xc]
100deef68:     	ldrh	w23, [x27, #0x10]
100deef6c:     	ldp	x26, x19, [x8, #0x8]
100deef70:     	ldr	wzr, [x8, #0x18]
100deef74:     	ldr	x21, [sp, #0x358]
100deef78:     	ldr	w1, [x21]
100deef7c:     	ldr	x0, [sp, #0x308]
100deef80:     	bl	0x100dbd988 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm17execution_contextNtB5_16ExecutionContext18function_is_strict>
100deef84:     	ldr	w8, [x21, #0x18]
100deef88:     	cmp	w8, w23
100deef8c:     	b.ls	0x100df1c3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x767c>
100deef90:     	mov	x27, x0
100deef94:     	ldr	x8, [x21, #0x10]
100deef98:     	add	x8, x8, x23, lsl #3
100deef9c:     	str	x8, [sp, #0x28]
100deefa0:     	ldr	x28, [x8]
100deefa4:     	mov	x8, #0x2                ; =2
100deefa8:     	movk	x8, #0xfffe, lsl #48
100deefac:     	and	x8, x28, x8
100deefb0:     	cmp	x28, #0x0
100deefb4:     	ccmp	x8, #0x0, #0x0, ne
100deefb8:     	ccmp	w28, #0x0, #0x4, eq
100deefbc:     	b.ne	0x100df0634 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6074>
100deefc0:     	ldr	x4, [sp, #0x358]
100deefc4:     	ldr	x8, [sp, #0x308]
100deefc8:     	ldr	x2, [x8]
100deefcc:     	ldr	w3, [x8, #0x10]
100deefd0:     	strb	w27, [sp, #0x8]
100deefd4:     	str	x19, [sp]
100deefd8:     	add	x0, sp, #0x420
100deefdc:     	mov	x1, x20
100deefe0:     	mov	x5, x24
100deefe4:     	mov	x6, x23
100deefe8:     	mov	x7, x26
100deefec:     	bl	0x100d0e4d8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm17property_dispatchNtB4_11Interpreter23run_delete_property_reg>
100deeff0:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100deeff4:     	mov	x3, x19
100deeff8:     	ldrh	w19, [x27, #0xc]
100deeffc:     	ldr	x8, [sp, #0x358]
100def000:     	ldr	x7, [x8, #0x28]
100def004:     	add	x0, sp, #0x420
100def008:     	mov	x1, x20
100def00c:     	ldr	x2, [sp, #0x328]
100def010:     	mov	w4, #0x3                ; =3
100def014:     	mov	w5, #0xa                ; =10
100def018:     	mov	w6, #0xa                ; =10
100def01c:     	bl	0x100df66fc <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm18runtime_activation16committed_valuesNtB9_11Interpreter12scalar_value>
100def020:     	b	0x100def480 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4ec0>
100def024:     	str	x25, [sp, #0x420]
100def028:     	str	x27, [sp, #0x428]
100def02c:     	add	x8, sp, #0x358
100def030:     	str	x8, [sp, #0x430]
100def034:     	ldr	x8, [sp, #0x340]
100def038:     	cbz	x8, 0x100df01d4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5c14>
100def03c:     	add	x0, sp, #0x420
100def040:     	bl	0x100caa868 <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickssi_0Bb_>
100def044:     	mov	x23, x0
100def048:     	mov	x0, x25
100def04c:     	mov	x1, x27
100def050:     	mov	w2, #0x1                ; =1
100def054:     	bl	0x100db4240 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock7operand>
100def058:     	tst	x0, #0xffff
100def05c:     	ldr	x1, [sp, #0x328]
100def060:     	b.eq	0x100df056c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5fac>
100def064:     	str	xzr, [sp, #0x290]
100def068:     	mov	w24, #0x0               ; =0
100def06c:     	b	0x100df070c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x614c>
100def070:     	ldp	w5, w6, [x27, #0xc]
100def074:     	ldr	w7, [x27, #0x14]
100def078:     	ldr	x4, [sp, #0x360]
100def07c:     	add	x0, sp, #0x420
100def080:     	mov	x1, x20
100def084:     	mov	x2, x19
100def088:     	ldr	x3, [sp, #0x328]
100def08c:     	bl	0x100d35a7c <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm17property_dispatch11jit_runtimeNtB6_11Interpreter29run_define_data_property_regs>
100def090:     	b	0x100def300 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4d40>
100def094:     	ldr	w6, [x27, #0xc]
100def098:     	ldr	x5, [sp, #0x360]
100def09c:     	ldr	x2, [x19]
100def0a0:     	ldr	w3, [x19, #0x10]
100def0a4:     	add	x0, sp, #0x420
100def0a8:     	mov	x1, x20
100def0ac:     	ldr	x4, [sp, #0x328]
100def0b0:     	bl	0x100cd2e30 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10module_opsNtB4_11Interpreter31run_mark_module_evaluated_const>
100def0b4:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100def0b8:     	ldrh	w8, [x27, #0x10]
100def0bc:     	ldr	x19, [sp, #0x358]
100def0c0:     	ldr	w9, [x19, #0x18]
100def0c4:     	cmp	w9, w8
100def0c8:     	b.ls	0x100df1730 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7170>
100def0cc:     	ldrh	w21, [x27, #0xc]
100def0d0:     	ldr	x9, [x19, #0x10]
100def0d4:     	add	x8, x9, x8, lsl #3
100def0d8:     	ldr	x23, [x8]
100def0dc:     	cmp	x23, #0x7
100def0e0:     	str	x8, [sp, #0x188]
100def0e4:     	b.eq	0x100df0278 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5cb8>
100def0e8:     	cmp	x23, #0x6
100def0ec:     	b.ne	0x100df0280 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5cc0>
100def0f0:     	mov	w0, #0x0                ; =0
100def0f4:     	b	0x100df02b4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5cf4>
100def0f8:     	ldp	w5, w6, [x27, #0xc]
100def0fc:     	ldr	x4, [sp, #0x360]
100def100:     	add	x0, sp, #0x420
100def104:     	mov	x1, x20
100def108:     	mov	x2, x19
100def10c:     	ldr	x3, [sp, #0x328]
100def110:     	bl	0x100cc74c8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10global_opsNtB4_11Interpreter25run_define_global_var_reg>
100def114:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100def118:     	ldr	x8, [x25, #0xe8]
100def11c:     	cmp	x8, x21
100def120:     	b.ls	0x100defdc4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5804>
100def124:     	ldr	x8, [x25, #0xe0]
100def128:     	mov	w9, #0x18               ; =24
100def12c:     	madd	x8, x21, x9, x8
100def130:     	ldrh	w9, [x8], #0x2
100def134:     	cmp	w9, #0x4
100def138:     	csel	x1, x8, xzr, eq
100def13c:     	b	0x100defdc8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5808>
100def140:     	ldrh	w9, [x27, #0xc]
100def144:     	ldr	x8, [sp, #0x358]
100def148:     	ldr	w10, [x8, #0x18]
100def14c:     	cmp	w10, w9
100def150:     	b.ls	0x100df17f4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7234>
100def154:     	ldr	x10, [x8, #0x10]
100def158:     	add	x12, x10, x9, lsl #3
100def15c:     	ldr	x9, [x12]
100def160:     	mov	x10, #0x2               ; =2
100def164:     	movk	x10, #0xfffe, lsl #48
100def168:     	and	x10, x9, x10
100def16c:     	mov	w11, #-0x3              ; =-3
100def170:     	and	x9, x9, x11
100def174:     	cmp	x10, #0x0
100def178:     	ccmp	x9, #0x0, #0x4, eq
100def17c:     	b.eq	0x100df1638 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7078>
100def180:     	adrp	x10, 0x1034f4000 <_global_added_lock+0x68>
100def184:     	add	x10, x10, #0xef8
100def188:     	ldr	x11, [x10]
100def18c:     	ldrb	w11, [x11, x9]
100def190:     	cmp	w11, #0x11
100def194:     	b.eq	0x100def1a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4be8>
100def198:     	ldr	x10, [x10]
100def19c:     	ldrb	w9, [x10, x9]
100def1a0:     	cmp	w9, #0x29
100def1a4:     	b.ne	0x100df1638 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7078>
100def1a8:     	str	x12, [sp, #0xb8]
100def1ac:     	b	0x100def710 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5150>
100def1b0:     	ldrh	w9, [x27, #0xc]
100def1b4:     	ldr	x8, [sp, #0x358]
100def1b8:     	ldrh	w11, [x8, #0x8]
100def1bc:     	ldr	x10, [x8, #0x10]
100def1c0:     	cmp	x10, #0x0
100def1c4:     	ccmp	w9, w11, #0x2, ne
100def1c8:     	b.hs	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100def1cc:     	ldr	x11, [x8, #0x30]
100def1d0:     	str	x11, [x10, x9, lsl #3]
100def1d4:     	b	0x100def710 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5150>
100def1d8:     	ldr	w5, [x27, #0xc]
100def1dc:     	ldrb	w8, [x27, #0x1d]
100def1e0:     	cmp	w8, #0x2
100def1e4:     	b.lo	0x100df04d4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5f14>
100def1e8:     	cmp	w8, #0x5
100def1ec:     	b.hs	0x100df04b8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5ef8>
100def1f0:     	ldr	w6, [x27, #0x10]
100def1f4:     	b	0x100df04d8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5f18>
100def1f8:     	ldp	w5, w6, [x27, #0xc]
100def1fc:     	ldr	x4, [sp, #0x360]
100def200:     	add	x0, sp, #0x420
100def204:     	mov	x1, x20
100def208:     	mov	x2, x19
100def20c:     	ldr	x3, [sp, #0x328]
100def210:     	bl	0x100ccebd8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10module_opsNtB4_11Interpreter25run_evaluate_module_const>
100def214:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100def218:     	ldrh	w9, [x27, #0xc]
100def21c:     	ldr	x8, [sp, #0x358]
100def220:     	ldrh	w10, [x8, #0x8]
100def224:     	ldr	x12, [x8, #0x10]
100def228:     	cmp	x12, #0x0
100def22c:     	ccmp	w10, #0x0, #0x4, ne
100def230:     	mov	w11, #0x8               ; =8
100def234:     	csel	x11, x11, x12, eq
100def238:     	ldr	x13, [x8, #0x30]
100def23c:     	mov	x14, #0x2               ; =2
100def240:     	movk	x14, #0xfffe, lsl #48
100def244:     	and	x14, x13, x14
100def248:     	mov	w15, #-0x3              ; =-3
100def24c:     	and	x13, x13, x15
100def250:     	cmp	x13, #0x0
100def254:     	ccmp	x14, #0x0, #0x0, ne
100def258:     	b.ne	0x100defbc4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5604>
100def25c:     	adrp	x14, 0x1034f4000 <_global_added_lock+0x68>
100def260:     	add	x14, x14, #0xef8
100def264:     	ldr	x15, [x14]
100def268:     	ldrb	w15, [x15, x13]
100def26c:     	cmp	w15, #0x23
100def270:     	b.ne	0x100defbc4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5604>
100def274:     	ldr	xzr, [x14]
100def278:     	ldr	x14, [x14]
100def27c:     	add	x13, x14, x13
100def280:     	ldr	x13, [x13, #0x10]
100def284:     	b	0x100defbc8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5608>
100def288:     	ldp	w5, w6, [x27, #0xc]
100def28c:     	ldr	w7, [x27, #0x14]
100def290:     	ldr	x4, [sp, #0x360]
100def294:     	add	x0, sp, #0x420
100def298:     	mov	x1, x20
100def29c:     	mov	x2, x19
100def2a0:     	ldr	x3, [sp, #0x328]
100def2a4:     	bl	0x100cffcd4 <__RNvMNtCsgTc3P8JF8aG_8otter_vm15jit_private_opsNtB4_11Interpreter19run_private_get_reg>
100def2a8:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100def2ac:     	ldrh	w8, [x27, #0xc]
100def2b0:     	ldr	x9, [sp, #0x358]
100def2b4:     	ldr	w11, [x9, #0x18]
100def2b8:     	cmp	w11, w8
100def2bc:     	b.ls	0x100df17a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x71e8>
100def2c0:     	ldrh	w10, [x27, #0x10]
100def2c4:     	cmp	w11, w10
100def2c8:     	ldr	x2, [sp, #0x328]
100def2cc:     	b.ls	0x100df1964 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x73a4>
100def2d0:     	ldr	x9, [x9, #0x10]
100def2d4:     	add	x8, x9, x8, lsl #3
100def2d8:     	str	x8, [sp, #0x100]
100def2dc:     	ldr	x5, [x8]
100def2e0:     	add	x8, x9, x10, lsl #3
100def2e4:     	str	x8, [sp, #0xd8]
100def2e8:     	ldr	x6, [x8]
100def2ec:     	add	x0, sp, #0x420
100def2f0:     	mov	x1, x20
100def2f4:     	mov	x3, x19
100def2f8:     	mov	w4, #0x13               ; =19
100def2fc:     	bl	0x100df6d00 <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm18runtime_activation16committed_valuesNtB9_11Interpreter21object_protocol_value>
100def300:     	ldr	x8, [sp, #0x420]
100def304:     	cmp	x8, #0x2
100def308:     	b.eq	0x100def70c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x514c>
100def30c:     	b	0x100df0eb0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68f0>
100def310:     	ldp	w5, w6, [x27, #0xc]
100def314:     	ldr	w7, [x27, #0x14]
100def318:     	ldr	x4, [sp, #0x360]
100def31c:     	ldr	x8, [x19]
100def320:     	ldr	x2, [x8, #0x30]
100def324:     	add	x0, sp, #0x420
100def328:     	mov	x1, x20
100def32c:     	ldr	x3, [sp, #0x328]
100def330:     	bl	0x100cfe554 <__RNvMNtCsgTc3P8JF8aG_8otter_vm14collection_opsNtB4_11Interpreter23run_new_collection_regs>
100def334:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100def338:     	ldp	w4, w5, [x27, #0xc]
100def33c:     	ldr	x3, [sp, #0x360]
100def340:     	add	x0, sp, #0x420
100def344:     	mov	x1, x20
100def348:     	ldr	x2, [sp, #0x328]
100def34c:     	bl	0x100cfbe30 <__RNvMNtCsgTc3P8JF8aG_8otter_vm14allocation_opsNtB4_11Interpreter21run_new_weak_ref_regs>
100def350:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100def354:     	ldp	w4, w5, [x27, #0xc]
100def358:     	ldr	x3, [sp, #0x358]
100def35c:     	ldr	x8, [x19]
100def360:     	ldr	x2, [x8, #0x30]
100def364:     	add	x0, sp, #0x420
100def368:     	mov	x1, x20
100def36c:     	bl	0x100d080c8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm15static_load_opsNtB4_11Interpreter19run_symbol_load_reg>
100def370:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100def374:     	ldrh	w8, [x27, #0x10]
100def378:     	ldr	x9, [sp, #0x358]
100def37c:     	ldr	w10, [x9, #0x18]
100def380:     	cmp	w10, w8
100def384:     	b.ls	0x100df17cc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x720c>
100def388:     	ldrh	w19, [x27, #0xc]
100def38c:     	ldr	x9, [x9, #0x10]
100def390:     	ldp	w6, w7, [x27, #0x14]
100def394:     	add	x8, x9, x8, lsl #3
100def398:     	str	x8, [sp, #0x130]
100def39c:     	ldr	x5, [x8]
100def3a0:     	ldr	w4, [sp, #0x36c]
100def3a4:     	ldr	x8, [sp, #0x308]
100def3a8:     	ldr	x2, [x8]
100def3ac:     	ldr	w3, [x8, #0x10]
100def3b0:     	add	x0, sp, #0x420
100def3b4:     	mov	x1, x20
100def3b8:     	bl	0x100cd7a40 <__RNvMNtCsgTc3P8JF8aG_8otter_vm11context_opsNtB4_11Interpreter26delete_lookup_global_value>
100def3bc:     	ldrb	w8, [sp, #0x420]
100def3c0:     	cmp	w8, #0xff
100def3c4:     	b.eq	0x100def48c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4ecc>
100def3c8:     	b	0x100df1464 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6ea4>
100def3cc:     	ldr	w5, [x27, #0xc]
100def3d0:     	ldrb	w8, [x27, #0x1d]
100def3d4:     	cmp	w8, #0x3
100def3d8:     	b.lo	0x100df0514 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5f54>
100def3dc:     	cmp	w8, #0x5
100def3e0:     	b.hs	0x100df04f4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5f34>
100def3e4:     	ldr	w9, [x27, #0x14]
100def3e8:     	b	0x100df050c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5f4c>
100def3ec:     	ldr	w23, [x27, #0xc]
100def3f0:     	mov	w19, #0x1               ; =1
100def3f4:     	mov	x0, x25
100def3f8:     	mov	x1, x27
100def3fc:     	mov	w2, #0x1                ; =1
100def400:     	bl	0x100db4240 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock7operand>
100def404:     	and	x8, x0, #0xffff
100def408:     	cmp	x8, #0x1
100def40c:     	b.ne	0x100df1808 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7248>
100def410:     	lsr	x6, x0, #32
100def414:     	ldr	x4, [sp, #0x360]
100def418:     	add	x0, sp, #0x420
100def41c:     	mov	x1, x20
100def420:     	ldr	x2, [sp, #0x308]
100def424:     	ldr	x3, [sp, #0x328]
100def428:     	mov	x5, x23
100def42c:     	str	x6, [sp, #0x280]
100def430:     	bl	0x100ccb13c <__RNvMNtCsgTc3P8JF8aG_8otter_vm10global_opsNtB4_11Interpreter29run_global_binding_exists_reg>
100def434:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100def438:     	ldrh	w8, [x27, #0x10]
100def43c:     	ldr	x9, [sp, #0x358]
100def440:     	ldr	w10, [x9, #0x18]
100def444:     	cmp	w10, w8
100def448:     	b.ls	0x100df1820 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7260>
100def44c:     	ldrh	w19, [x27, #0xc]
100def450:     	ldr	x9, [x9, #0x10]
100def454:     	add	x8, x9, x8, lsl #3
100def458:     	str	x8, [sp, #0x118]
100def45c:     	ldr	x5, [x8]
100def460:     	add	x0, sp, #0x420
100def464:     	mov	x1, x20
100def468:     	ldr	x2, [sp, #0x328]
100def46c:     	ldr	x3, [sp, #0x308]
100def470:     	mov	w4, #0x2                ; =2
100def474:     	mov	w6, #0xa                ; =10
100def478:     	mov	w7, #0xa                ; =10
100def47c:     	bl	0x100df66fc <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm18runtime_activation16committed_valuesNtB9_11Interpreter12scalar_value>
100def480:     	ldr	x8, [sp, #0x420]
100def484:     	cmp	x8, #0x2
100def488:     	b.ne	0x100df0ec4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6904>
100def48c:     	ldr	x8, [sp, #0x358]
100def490:     	ldrh	w10, [x8, #0x8]
100def494:     	ldr	x9, [x8, #0x10]
100def498:     	cmp	w10, w19
100def49c:     	ccmp	x9, #0x0, #0x4, hi
100def4a0:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100def4a4:     	ldr	x10, [sp, #0x428]
100def4a8:     	str	x10, [x9, x19, lsl #3]
100def4ac:     	b	0x100def710 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5150>
100def4b0:     	ldp	w5, w6, [x27, #0xc]
100def4b4:     	ldr	x4, [sp, #0x360]
100def4b8:     	add	x0, sp, #0x420
100def4bc:     	mov	x1, x20
100def4c0:     	mov	x2, x19
100def4c4:     	ldr	x3, [sp, #0x328]
100def4c8:     	bl	0x100ccac38 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10global_opsNtB4_11Interpreter28run_load_global_or_throw_reg>
100def4cc:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100def4d0:     	mov	x0, x25
100def4d4:     	mov	x1, x27
100def4d8:     	mov	x2, #0x0                ; =0
100def4dc:     	bl	0x100db41e8 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock3reg>
100def4e0:     	mov	x23, x0
100def4e4:     	mov	x0, x25
100def4e8:     	mov	x1, x27
100def4ec:     	mov	w2, #0x1                ; =1
100def4f0:     	bl	0x100db41e8 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock3reg>
100def4f4:     	mov	x24, x0
100def4f8:     	mov	x0, x25
100def4fc:     	mov	x1, x27
100def500:     	mov	w2, #0x2                ; =2
100def504:     	bl	0x100db41e8 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock3reg>
100def508:     	mov	x26, x0
100def50c:     	mov	x0, x25
100def510:     	mov	x1, x27
100def514:     	mov	w2, #0x3                ; =3
100def518:     	bl	0x100db41e8 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock3reg>
100def51c:     	mov	x28, x0
100def520:     	mov	x0, x25
100def524:     	mov	x1, x27
100def528:     	mov	w2, #0x4                ; =4
100def52c:     	bl	0x100db4320 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock8register>
100def530:     	ldr	x3, [sp, #0x360]
100def534:     	strh	w1, [sp, #0x2]
100def538:     	strh	w0, [sp]
100def53c:     	add	x0, sp, #0x420
100def540:     	mov	x1, x20
100def544:     	ldr	x2, [sp, #0x328]
100def548:     	mov	x4, x23
100def54c:     	mov	x5, x24
100def550:     	mov	x6, x26
100def554:     	mov	x7, x28
100def558:     	bl	0x100cda2b8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm12function_opsNtB4_11Interpreter19run_make_class_regs>
100def55c:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100def560:     	ldp	w3, w4, [x27, #0xc]
100def564:     	ldr	x2, [sp, #0x360]
100def568:     	add	x0, sp, #0x420
100def56c:     	ldr	x1, [sp, #0x328]
100def570:     	bl	0x100d16cf4 <__RNvMNtCsgTc3P8JF8aG_8otter_vm19jit_class_value_opsNtB4_11Interpreter25run_is_eval_intrinsic_reg>
100def574:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100def578:     	ldrh	w8, [x27, #0x10]
100def57c:     	ldr	x19, [sp, #0x358]
100def580:     	ldr	w9, [x19, #0x18]
100def584:     	cmp	w9, w8
100def588:     	b.ls	0x100df18e8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7328>
100def58c:     	ldrh	w21, [x27, #0xc]
100def590:     	ldp	w5, w6, [x27, #0x14]
100def594:     	ldr	x9, [x19, #0x10]
100def598:     	add	x22, x9, x8, lsl #3
100def59c:     	ldr	x4, [x22]
100def5a0:     	ldr	w3, [sp, #0x36c]
100def5a4:     	ldr	x8, [sp, #0x308]
100def5a8:     	ldr	x1, [x8]
100def5ac:     	ldr	w2, [x8, #0x10]
100def5b0:     	add	x0, sp, #0x420
100def5b4:     	bl	0x100cd73a8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm11context_opsNtB4_11Interpreter24resolve_lookup_ref_value>
100def5b8:     	ldrb	w8, [sp, #0x420]
100def5bc:     	cmp	w8, #0xff
100def5c0:     	b.ne	0x100df1464 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6ea4>
100def5c4:     	ldrh	w9, [x19, #0x8]
100def5c8:     	ldr	x8, [x19, #0x10]
100def5cc:     	cmp	w9, w21
100def5d0:     	ccmp	x8, #0x0, #0x4, hi
100def5d4:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100def5d8:     	str	x22, [sp, #0xa0]
100def5dc:     	ldr	x9, [sp, #0x428]
100def5e0:     	str	x9, [x8, x21, lsl #3]
100def5e4:     	ldr	w8, [x19, #0x4]
100def5e8:     	cmn	w8, #0x1
100def5ec:     	b.ne	0x100df05d0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6010>
100def5f0:     	b	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100def5f4:     	mov	x5, #0x0                ; =0
100def5f8:     	ldr	x3, [sp, #0x328]
100def5fc:     	ldr	x6, [sp, #0x218]
100def600:     	ldr	x4, [sp, #0x360]
100def604:     	add	x0, sp, #0x420
100def608:     	mov	x1, x20
100def60c:     	ldr	x2, [sp, #0x308]
100def610:     	str	x6, [sp, #0x218]
100def614:     	bl	0x100d081d8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm16arguments_accessNtB4_11Interpreter20read_frame_arguments>
100def618:     	ldr	x8, [sp, #0x420]
100def61c:     	cmp	x8, #0x2
100def620:     	b.ne	0x100df0ec4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6904>
100def624:     	ldr	x8, [sp, #0x358]
100def628:     	ldrh	w9, [x8, #0x8]
100def62c:     	ldr	x8, [x8, #0x10]
100def630:     	cmp	w9, w19
100def634:     	ccmp	x8, #0x0, #0x4, hi
100def638:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100def63c:     	ldr	x9, [sp, #0x428]
100def640:     	str	x9, [x8, x19, lsl #3]
100def644:     	b	0x100def70c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x514c>
100def648:     	ldp	w5, w6, [x27, #0xc]
100def64c:     	ldr	x4, [sp, #0x358]
100def650:     	add	x0, sp, #0x420
100def654:     	mov	x1, x20
100def658:     	ldr	x2, [sp, #0x328]
100def65c:     	mov	x3, x19
100def660:     	bl	0x100e37110 <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB6_11Interpreter20run_bitwise_not_regs>
100def664:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100def668:     	ldp	w3, w4, [x27, #0xc]
100def66c:     	ldr	w5, [x27, #0x14]
100def670:     	ldr	x2, [sp, #0x358]
100def674:     	add	x0, sp, #0x420
100def678:     	mov	x1, x20
100def67c:     	bl	0x100da8490 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6string3opsNtB6_11Interpreter25run_get_string_index_regs>
100def680:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100def684:     	ldp	w5, w6, [x27, #0xc]
100def688:     	ldr	x4, [sp, #0x358]
100def68c:     	str	x26, [sp]
100def690:     	add	x0, sp, #0x420
100def694:     	mov	x1, x20
100def698:     	ldr	x2, [sp, #0x328]
100def69c:     	mov	x3, x19
100def6a0:     	mov	x7, x28
100def6a4:     	bl	0x100e354d0 <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB6_11Interpreter12run_neg_regs>
100def6a8:     	b	0x100defb2c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x556c>
100def6ac:     	ldrh	w8, [x27, #0xc]
100def6b0:     	ldr	x9, [sp, #0x358]
100def6b4:     	ldr	w11, [x9, #0x18]
100def6b8:     	cmp	w11, w8
100def6bc:     	b.ls	0x100df15c0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7000>
100def6c0:     	ldrh	w10, [x27, #0x10]
100def6c4:     	ldr	x12, [sp, #0x1c8]
100def6c8:     	str	x12, [sp, #0x1e8]
100def6cc:     	cmp	w11, w10
100def6d0:     	b.ls	0x100df15c0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7000>
100def6d4:     	ldr	w3, [x27, #0x14]
100def6d8:     	ldr	x9, [x9, #0x10]
100def6dc:     	add	x8, x9, x8, lsl #3
100def6e0:     	str	x8, [sp, #0x1e8]
100def6e4:     	ldr	x4, [x8]
100def6e8:     	add	x8, x9, x10, lsl #3
100def6ec:     	str	x8, [sp, #0x1c8]
100def6f0:     	ldr	x2, [x8]
100def6f4:     	add	x0, sp, #0x420
100def6f8:     	mov	x1, x20
100def6fc:     	bl	0x100cd7c1c <__RNvMNtCsgTc3P8JF8aG_8otter_vm11context_opsNtB4_11Interpreter28bind_this_context_slot_value>
100def700:     	ldrb	w8, [sp, #0x420]
100def704:     	cmp	w8, #0xff
100def708:     	b.ne	0x100df0e20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6860>
100def70c:     	ldr	x8, [sp, #0x358]
100def710:     	ldr	w9, [x8, #0x4]
100def714:     	cmn	w9, #0x1
100def718:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100def71c:     	add	w9, w9, #0x1
100def720:     	str	w9, [x8, #0x4]
100def724:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100def728:     	add	x8, sp, #0x358
100def72c:     	str	x8, [sp, #0x420]
100def730:     	str	x25, [sp, #0x428]
100def734:     	str	x27, [sp, #0x430]
100def738:     	str	x20, [sp, #0x438]
100def73c:     	ldr	x8, [sp, #0x340]
100def740:     	cbz	x8, 0x100df01e0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5c20>
100def744:     	add	x0, sp, #0x420
100def748:     	bl	0x100caa030 <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickss5_0Bb_>
100def74c:     	mov	x23, x0
100def750:     	ldr	w1, [x27]
100def754:     	mov	x0, x25
100def758:     	bl	0x100cf6e08 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter28record_call_attempt_feedback>
100def75c:     	add	x0, sp, #0x420
100def760:     	mov	x1, x20
100def764:     	ldr	x2, [sp, #0x328]
100def768:     	ldr	x3, [sp, #0x308]
100def76c:     	mov	x4, x25
100def770:     	mov	x5, x27
100def774:     	bl	0x100de1940 <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter30do_call_forward_arguments_exec>
100def778:     	ldr	x8, [sp, #0x420]
100def77c:     	cmp	x8, #0x2
100def780:     	b.ne	0x100df18fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x733c>
100def784:     	ldr	x8, [sp, #0x328]
100def788:     	ldr	x1, [x8, #0x128]
100def78c:     	ldrb	w0, [x8, #0x112]
100def790:     	bl	0x100ddfe3c <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter22staged_bytecode_target>
100def794:     	cmp	w0, #0x1
100def798:     	ldr	x8, [sp, #0x40]
100def79c:     	b.ne	0x100df0370 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5db0>
100def7a0:     	mov	x23, #0x0               ; =0
100def7a4:     	bfi	x8, x1, #24, #40
100def7a8:     	str	x8, [sp, #0x40]
100def7ac:     	and	x8, x8, #0xffffffffffffff
100def7b0:     	b	0x100df0380 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5dc0>
100def7b4:     	mov	x8, x19
100def7b8:     	ldrh	w19, [x27, #0xc]
100def7bc:     	ldr	w3, [x27, #0x10]
100def7c0:     	ldr	x2, [x8]
100def7c4:     	add	x0, sp, #0x420
100def7c8:     	mov	x1, x20
100def7cc:     	bl	0x100e02790 <__RNvMs4_CsgTc3P8JF8aG_8otter_vmNtB5_11Interpreter26load_string_constant_value>
100def7d0:     	ldrb	w8, [sp, #0x420]
100def7d4:     	cmp	w8, #0xff
100def7d8:     	b.eq	0x100def48c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4ecc>
100def7dc:     	b	0x100df1494 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6ed4>
100def7e0:     	ldrh	w9, [x27, #0xc]
100def7e4:     	ldr	x8, [sp, #0x358]
100def7e8:     	ldr	w12, [x8, #0x18]
100def7ec:     	cmp	w12, w9
100def7f0:     	b.ls	0x100df1758 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7198>
100def7f4:     	ldrh	w10, [x27, #0x10]
100def7f8:     	cmp	w12, w10
100def7fc:     	ldr	x21, [sp, #0x328]
100def800:     	b.ls	0x100df19a0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x73e0>
100def804:     	ldrh	w11, [x27, #0x14]
100def808:     	cmp	w12, w11
100def80c:     	b.ls	0x100df1a7c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x74bc>
100def810:     	ldr	x12, [x8, #0x10]
100def814:     	add	x9, x12, x9, lsl #3
100def818:     	str	x9, [sp, #0x98]
100def81c:     	ldr	x23, [x9]
100def820:     	add	x9, x12, x10, lsl #3
100def824:     	str	x9, [sp, #0x88]
100def828:     	ldr	x19, [x9]
100def82c:     	add	x9, x12, x11, lsl #3
100def830:     	str	x9, [sp, #0x70]
100def834:     	ldr	x24, [x9]
100def838:     	ldr	w1, [x8]
100def83c:     	ldr	x22, [sp, #0x308]
100def840:     	mov	x0, x22
100def844:     	bl	0x100dbd988 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm17execution_contextNtB5_16ExecutionContext18function_is_strict>
100def848:     	str	x19, [sp, #0x428]
100def84c:     	mov	w8, #-0x1               ; =-1
100def850:     	str	w8, [sp, #0x420]
100def854:     	ldr	x4, [sp, #0x360]
100def858:     	strb	w0, [sp]
100def85c:     	sub	x0, x29, #0xa8
100def860:     	add	x6, sp, #0x420
100def864:     	mov	x1, x20
100def868:     	mov	x2, x22
100def86c:     	mov	x3, x21
100def870:     	mov	x5, x23
100def874:     	mov	x7, x24
100def878:     	bl	0x100d0fa40 <__RNvMNtCsgTc3P8JF8aG_8otter_vm17property_dispatchNtB4_11Interpreter24run_store_super_property>
100def87c:     	ldur	x8, [x29, #-0xa8]
100def880:     	cmp	x8, #0x2
100def884:     	b.eq	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100def888:     	b	0x100df13fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e3c>
100def88c:     	ldr	x8, [sp, #0x340]
100def890:     	cbz	x8, 0x100df020c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5c4c>
100def894:     	ldr	w1, [x27]
100def898:     	mov	x0, x25
100def89c:     	bl	0x100cf6e08 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter28record_call_attempt_feedback>
100def8a0:     	ldr	w24, [x27, #0x4]
100def8a4:     	mov	w8, #-0x1               ; =-1
100def8a8:     	cmp	x24, x8
100def8ac:     	cset	w19, ne
100def8b0:     	cmn	w24, #0x1
100def8b4:     	b.eq	0x100df059c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5fdc>
100def8b8:     	ldr	x8, [x20, #0x7420]
100def8bc:     	cmp	x8, x24
100def8c0:     	b.ls	0x100def8dc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x531c>
100def8c4:     	ldr	x8, [x20, #0x7418]
100def8c8:     	ldrb	w8, [x8, x24]
100def8cc:     	ldr	x9, [x20, #0x7438]
100def8d0:     	cmp	w8, #0x0
100def8d4:     	ccmp	x9, x24, #0x0, ne
100def8d8:     	b.hi	0x100df0784 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x61c4>
100def8dc:     	mov	w21, #0x1               ; =1
100def8e0:     	b	0x100df079c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x61dc>
100def8e4:     	ldrh	w8, [x27, #0x10]
100def8e8:     	ldr	x19, [sp, #0x358]
100def8ec:     	ldr	w9, [x19, #0x18]
100def8f0:     	cmp	w9, w8
100def8f4:     	b.ls	0x100df18d4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7314>
100def8f8:     	ldrh	w21, [x27, #0xc]
100def8fc:     	ldr	x9, [x19, #0x10]
100def900:     	add	x22, x9, x8, lsl #3
100def904:     	ldr	x23, [x22]
100def908:     	cmp	x23, #0x6
100def90c:     	b.eq	0x100df05a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5fe8>
100def910:     	cmp	x23, #0x7
100def914:     	b.eq	0x100def954 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5394>
100def918:     	mov	x8, #0x2                ; =2
100def91c:     	movk	x8, #0xfffe, lsl #48
100def920:     	and	x8, x23, x8
100def924:     	cmp	x23, #0x0
100def928:     	ccmp	x8, #0x0, #0x0, ne
100def92c:     	b.eq	0x100def948 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5388>
100def930:     	mov	x0, x23
100def934:     	bl	0x100d255a8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm5valueNtB2_5Value15to_boolean_pure>
100def938:     	ands	w8, w0, #0xff
100def93c:     	b.eq	0x100df05a4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5fe4>
100def940:     	cmp	w8, #0x2
100def944:     	b.ne	0x100def954 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5394>
100def948:     	mov	x0, x23
100def94c:     	bl	0x100d253e8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm5valueNtB2_5Value15to_boolean_cell>
100def950:     	cbz	w0, 0x100df05a4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5fe4>
100def954:     	mov	w23, #0x7               ; =7
100def958:     	b	0x100df05a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5fe8>
100def95c:     	ldr	w8, [x27, #0x10]
100def960:     	ldr	x9, [x19]
100def964:     	ldr	x9, [x9, #0x20]
100def968:     	ldr	x10, [x9, #0x68]
100def96c:     	cmp	x10, x8
100def970:     	b.ls	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100def974:     	ldr	x9, [x9, #0x60]
100def978:     	mov	w10, #0x30              ; =48
100def97c:     	umaddl	x8, w8, w10, x9
100def980:     	ldr	x9, [x8]
100def984:     	mov	x10, #-0x7fffffffffffffff ; =-9223372036854775807
100def988:     	cmp	x9, x10
100def98c:     	b.ne	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100def990:     	ldr	d0, [x8, #0x8]
100def994:     	fmov	x8, d0
100def998:     	fcmp	d0, #0.0
100def99c:     	b.ne	0x100df0220 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5c60>
100def9a0:     	mov	x9, #-0x8000000000000000 ; =-9223372036854775808
100def9a4:     	cmp	x8, x9
100def9a8:     	b.ne	0x100df022c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5c6c>
100def9ac:     	b	0x100df0254 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5c94>
100def9b0:     	add	x0, sp, #0x420
100def9b4:     	mov	x1, x20
100def9b8:     	mov	x2, x19
100def9bc:     	ldr	x3, [sp, #0x328]
100def9c0:     	mov	x4, x25
100def9c4:     	mov	x5, x27
100def9c8:     	bl	0x100c001a8 <__RINvMNtCsgTc3P8JF8aG_8otter_vm11promise_opsNtB5_11Interpreter24run_promise_new_operandsNtNtB5_10executable11OperandViewEB5_>
100def9cc:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100def9d0:     	ldr	x2, [sp, #0x358]
100def9d4:     	str	x26, [sp]
100def9d8:     	add	x0, sp, #0x420
100def9dc:     	mov	x1, x20
100def9e0:     	mov	x3, x23
100def9e4:     	mov	x4, x6
100def9e8:     	mov	x5, x7
100def9ec:     	mov	w6, #0x0                ; =0
100def9f0:     	mov	x7, x28
100def9f4:     	bl	0x100e35cd0 <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB6_11Interpreter14run_equal_regs>
100def9f8:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100def9fc:     	ldr	x8, [sp, #0x358]
100defa00:     	ldr	w9, [x8, #0x18]
100defa04:     	cmp	w9, w6
100defa08:     	b.ls	0x100df1924 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7364>
100defa0c:     	cmp	w9, w7
100defa10:     	b.ls	0x100df1a0c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x744c>
100defa14:     	ldr	x8, [x8, #0x10]
100defa18:     	add	x9, x8, x6, lsl #3
100defa1c:     	str	x9, [sp, #0xe8]
100defa20:     	ldr	x5, [x9]
100defa24:     	add	x8, x8, x7, lsl #3
100defa28:     	str	x8, [sp, #0xc0]
100defa2c:     	ldr	x6, [x8]
100defa30:     	add	x0, sp, #0x420
100defa34:     	mov	x1, x20
100defa38:     	mov	x3, x19
100defa3c:     	mov	w4, #0x4                ; =4
100defa40:     	mov	w7, #0xa                ; =10
100defa44:     	bl	0x100df66fc <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm18runtime_activation16committed_valuesNtB9_11Interpreter12scalar_value>
100defa48:     	ldr	x8, [sp, #0x420]
100defa4c:     	cmp	x8, #0x2
100defa50:     	b.ne	0x100df0ec4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6904>
100defa54:     	ldr	x8, [sp, #0x358]
100defa58:     	ldrh	w10, [x8, #0x8]
100defa5c:     	ldr	x9, [x8, #0x10]
100defa60:     	cmp	w10, w23
100defa64:     	ccmp	x9, #0x0, #0x4, hi
100defa68:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100defa6c:     	ldr	x10, [sp, #0x428]
100defa70:     	str	x10, [x9, x23, lsl #3]
100defa74:     	ldr	w9, [x8, #0x4]
100defa78:     	b	0x100def71c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x515c>
100defa7c:     	ldr	x4, [sp, #0x358]
100defa80:     	stp	x28, x26, [sp, #0x8]
100defa84:     	add	x0, sp, #0x420
100defa88:     	strb	wzr, [sp]
100defa8c:     	mov	x1, x20
100defa90:     	mov	x3, x19
100defa94:     	mov	x5, x23
100defa98:     	bl	0x100e371ac <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB6_11Interpreter20run_loose_equal_regs>
100defa9c:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100defaa0:     	stp	x28, x26, [sp]
100defaa4:     	add	x0, sp, #0x420
100defaa8:     	mov	x1, x20
100defaac:     	mov	x3, x19
100defab0:     	bl	0x100c38830 <__RINvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB7_11Interpreter16run_numeric_regsNvNtNtB7_6number7bitwise9shr_arithEB7_>
100defab4:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100defab8:     	stp	x28, x26, [sp]
100defabc:     	add	x0, sp, #0x420
100defac0:     	mov	x1, x20
100defac4:     	mov	x3, x19
100defac8:     	bl	0x100c38374 <__RINvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB7_11Interpreter16run_numeric_regsNvNtNtB7_6number7bitwise3shlEB7_>
100defacc:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100defad0:     	stp	x28, x26, [sp]
100defad4:     	add	x0, sp, #0x420
100defad8:     	mov	x1, x20
100defadc:     	mov	x3, x19
100defae0:     	bl	0x100c374e0 <__RINvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB7_11Interpreter16run_numeric_regsNvNtNtB7_6number7bitwise10bitwise_orEB7_>
100defae4:     	b	0x100defb2c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x556c>
100defae8:     	stp	x28, x26, [sp]
100defaec:     	add	x0, sp, #0x420
100defaf0:     	mov	x1, x20
100defaf4:     	mov	x3, x19
100defaf8:     	bl	0x100c379bc <__RINvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB7_11Interpreter16run_numeric_regsNvNtNtB7_6number7bitwise11bitwise_andEB7_>
100defafc:     	b	0x100defb2c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x556c>
100defb00:     	stp	x28, x26, [sp]
100defb04:     	add	x0, sp, #0x420
100defb08:     	mov	x1, x20
100defb0c:     	mov	x3, x19
100defb10:     	bl	0x100c37e98 <__RINvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB7_11Interpreter16run_numeric_regsNvNtNtB7_6number7bitwise11bitwise_xorEB7_>
100defb14:     	b	0x100defb2c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x556c>
100defb18:     	stp	x28, x26, [sp]
100defb1c:     	add	x0, sp, #0x420
100defb20:     	mov	x1, x20
100defb24:     	mov	x3, x19
100defb28:     	bl	0x100e34fd8 <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB6_11Interpreter12run_add_regs>
100defb2c:     	ldr	x8, [sp, #0x420]
100defb30:     	cmp	x8, #0x2
100defb34:     	b.eq	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100defb38:     	b	0x100df1344 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6d84>
100defb3c:     	str	w11, [sp, #0x348]
100defb40:     	ldr	x4, [sp, #0x360]
100defb44:     	add	x0, sp, #0x420
100defb48:     	mov	x1, x20
100defb4c:     	mov	x2, x19
100defb50:     	ldr	x3, [sp, #0x328]
100defb54:     	mov	x5, x24
100defb58:     	bl	0x100e32908 <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm15iterator_recordNtB6_11Interpreter22step_fast_array_record>
100defb5c:     	ldr	x8, [sp, #0x420]
100defb60:     	cmp	x8, #0x2
100defb64:     	b.ne	0x100df0ec4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6904>
100defb68:     	ldr	x8, [sp, #0x358]
100defb6c:     	ldr	w10, [x21]
100defb70:     	ldrh	w9, [x8, #0x8]
100defb74:     	ldr	x11, [x8, #0x10]
100defb78:     	cmp	w9, w10, uxth
100defb7c:     	ccmp	x11, #0x0, #0x4, hi
100defb80:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100defb84:     	ldr	x12, [sp, #0x428]
100defb88:     	ldrb	w9, [sp, #0x430]
100defb8c:     	and	x10, x10, #0xffff
100defb90:     	str	x12, [x11, x10, lsl #3]
100defb94:     	ldr	w10, [x27, #0x10]
100defb98:     	ldrh	w11, [x8, #0x8]
100defb9c:     	cmp	w11, w10, uxth
100defba0:     	b.ls	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100defba4:     	ldr	x11, [x8, #0x10]
100defba8:     	cbz	x11, 0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100defbac:     	and	x10, x10, #0xffff
100defbb0:     	cmp	w9, #0x0
100defbb4:     	mov	w9, #0x6                ; =6
100defbb8:     	cinc	x9, x9, ne
100defbbc:     	str	x9, [x11, x10, lsl #3]
100defbc0:     	b	0x100def710 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5150>
100defbc4:     	mov	w13, #0xa               ; =10
100defbc8:     	cmp	x12, #0x0
100defbcc:     	ccmp	w9, w10, #0x2, ne
100defbd0:     	b.hs	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100defbd4:     	str	x13, [x11, x9, lsl #3]
100defbd8:     	b	0x100def710 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5150>
100defbdc:     	mov	w19, #0xff              ; =255
100defbe0:     	b	0x100defcf8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5738>
100defbe4:     	mov	x8, #0x0                ; =0
100defbe8:     	ldp	w5, w6, [x27, #0xc]
100defbec:     	ldr	x4, [sp, #0x360]
100defbf0:     	stp	x8, x9, [sp]
100defbf4:     	add	x0, sp, #0x420
100defbf8:     	sub	x7, x29, #0xc0
100defbfc:     	mov	x1, x20
100defc00:     	ldr	x2, [sp, #0x308]
100defc04:     	bl	0x100d33f50 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm17property_dispatch10propertiesNtB6_11Interpreter26run_has_named_property_reg>
100defc08:     	ldr	x8, [sp, #0x420]
100defc0c:     	cmp	x8, #0x2
100defc10:     	b.ne	0x100df0eb0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68f0>
100defc14:     	lsr	x8, x19, #8
100defc18:     	str	x8, [sp, #0xb0]
100defc1c:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100defc20:     	mov	w19, #0xa               ; =10
100defc24:     	ldr	x23, [x20, #0x74c8]
100defc28:     	ldr	x8, [x20, #0x74b8]
100defc2c:     	cmp	x23, x8
100defc30:     	b.ne	0x100defc40 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5680>
100defc34:     	mov	w8, #0x74b8             ; =29880
100defc38:     	add	x0, x20, x8
100defc3c:     	bl	0x101b65040 <__RNvMs4_NtCs6zY7rJBZy9D_5alloc7raw_vecINtB5_6RawVecTjNtNtCsgTc3P8JF8aG_8otter_vm10executable19FeedbackSlotAddressEE8grow_oneBS_>
100defc40:     	ldr	x8, [x20, #0x74c0]
100defc44:     	str	x19, [x8, x23, lsl #3]
100defc48:     	add	x8, x23, #0x1
100defc4c:     	str	x8, [x20, #0x74c8]
100defc50:     	add	x0, sp, #0x420
100defc54:     	mov	x1, x20
100defc58:     	ldr	x2, [sp, #0x328]
100defc5c:     	ldr	x3, [sp, #0x308]
100defc60:     	mov	x4, x24
100defc64:     	mov	x5, x26
100defc68:     	bl	0x100d28584 <__RNvMNtCsgTc3P8JF8aG_8otter_vm9async_opsNtB4_11Interpreter8do_await>
100defc6c:     	ldr	x1, [x20, #0x74c8]
100defc70:     	cmp	x23, x1
100defc74:     	b.hs	0x100df1e98 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x78d8>
100defc78:     	ldr	x8, [x20, #0x74c0]
100defc7c:     	ldr	x19, [x8, x23, lsl #3]
100defc80:     	str	x23, [x20, #0x74c8]
100defc84:     	ldr	x8, [sp, #0x420]
100defc88:     	cmp	x8, #0x2
100defc8c:     	ldr	x0, [sp, #0x328]
100defc90:     	ldr	x1, [sp, #0x2c8]
100defc94:     	b.ne	0x100df161c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x705c>
100defc98:     	bl	0x100df415c <__RNvMs2_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStack11is_at_floor>
100defc9c:     	tbz	w0, #0x0, 0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100defca0:     	b	0x100df1b18 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7558>
100defca4:     	mov	w19, #0xff              ; =255
100defca8:     	mov	x0, x25
100defcac:     	mov	x1, x27
100defcb0:     	mov	w2, #0x1                ; =1
100defcb4:     	bl	0x100db4240 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock7operand>
100defcb8:     	tst	x0, #0xffff
100defcbc:     	b.ne	0x100defcf8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5738>
100defcc0:     	lsr	x2, x0, #16
100defcc4:     	ldr	x0, [x21, #0x180]
100defcc8:     	ldr	x1, [sp, #0x360]
100defccc:     	bl	0x100ca9c7c <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickss2_0Bb_>
100defcd0:     	cmp	x0, #0x1
100defcd4:     	b.ne	0x100defcf8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5738>
100defcd8:     	mov	x0, x20
100defcdc:     	bl	0x100ca9d88 <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickss3_0Bb_>
100defce0:     	and	w21, w0, #0xff
100defce4:     	lsr	w8, w0, #8
100defce8:     	str	x8, [sp, #0x278]
100defcec:     	lsr	x8, x0, #32
100defcf0:     	str	x8, [sp, #0x2b0]
100defcf4:     	b	0x100defcfc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x573c>
100defcf8:     	mov	w21, #0xff              ; =255
100defcfc:     	cmp	w24, #0x42
100defd00:     	b.ne	0x100defd1c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x575c>
100defd04:     	add	x0, sp, #0x420
100defd08:     	ldr	x1, [sp, #0x328]
100defd0c:     	mov	x2, x25
100defd10:     	mov	x3, x27
100defd14:     	bl	0x100ddfa70 <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter22do_call_with_this_exec>
100defd18:     	b	0x100defd30 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5770>
100defd1c:     	add	x0, sp, #0x420
100defd20:     	ldr	x1, [sp, #0x328]
100defd24:     	mov	x2, x25
100defd28:     	mov	x3, x27
100defd2c:     	bl	0x100dde268 <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter13do_call_inner>
100defd30:     	ldrb	w8, [sp, #0x420]
100defd34:     	cmp	w8, #0xff
100defd38:     	b.ne	0x100df0e20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6860>
100defd3c:     	ldr	x8, [sp, #0x340]
100defd40:     	ldr	x22, [sp, #0x2b0]
100defd44:     	cbz	x8, 0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100defd48:     	cmp	w19, #0xff
100defd4c:     	b.ne	0x100df0348 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5d88>
100defd50:     	ldr	x8, [sp, #0x328]
100defd54:     	ldr	x1, [x8, #0x128]
100defd58:     	ldrb	w0, [x8, #0x112]
100defd5c:     	bl	0x100ddfe3c <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter22staged_bytecode_target>
100defd60:     	cmp	w0, #0x1
100defd64:     	b.ne	0x100df0320 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5d60>
100defd68:     	str	x1, [sp, #0x2b8]
100defd6c:     	mov	w19, #0x0               ; =0
100defd70:     	str	xzr, [sp, #0x2c0]
100defd74:     	b	0x100df0348 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5d88>
100defd78:     	str	xzr, [sp, #0x298]
100defd7c:     	mov	w24, #0x0               ; =0
100defd80:     	b	0x100df00c0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5b00>
100defd84:     	str	xzr, [sp, #0x2a0]
100defd88:     	mov	w24, #0x0               ; =0
100defd8c:     	b	0x100df015c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5b9c>
100defd90:     	mov	x1, #0x0                ; =0
100defd94:     	mov	x0, x20
100defd98:     	bl	0x100d130c0 <__RNvMNtCsgTc3P8JF8aG_8otter_vm18literal_allocationNtB4_11Interpreter22literal_site_placement>
100defd9c:     	mov	x4, x0
100defda0:     	mov	x5, x1
100defda4:     	ldr	x3, [sp, #0x360]
100defda8:     	add	x0, sp, #0x420
100defdac:     	mov	x1, x20
100defdb0:     	ldr	x2, [sp, #0x328]
100defdb4:     	mov	x6, x25
100defdb8:     	mov	x7, x27
100defdbc:     	bl	0x100c02134 <__RINvMNtCsgTc3P8JF8aG_8otter_vm14allocation_opsNtB5_11Interpreter22run_new_array_operandsNtNtB5_10executable11OperandViewEB5_>
100defdc0:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100defdc4:     	mov	x1, #0x0                ; =0
100defdc8:     	mov	x0, x20
100defdcc:     	bl	0x100d130c0 <__RNvMNtCsgTc3P8JF8aG_8otter_vm18literal_allocationNtB4_11Interpreter22literal_site_placement>
100defdd0:     	mov	x5, x0
100defdd4:     	mov	x6, x1
100defdd8:     	ldr	x3, [sp, #0x360]
100defddc:     	str	x27, [sp]
100defde0:     	add	x0, sp, #0x420
100defde4:     	mov	x1, x20
100defde8:     	ldr	x2, [sp, #0x328]
100defdec:     	ldr	x4, [sp, #0x308]
100defdf0:     	mov	x7, x25
100defdf4:     	bl	0x100c02e64 <__RINvMNtCsgTc3P8JF8aG_8otter_vm14allocation_opsNtB5_11Interpreter31run_new_object_literal_operandsNtNtB5_10executable11OperandViewEB5_>
100defdf8:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100defdfc:     	stp	x28, x26, [sp]
100defe00:     	add	x0, sp, #0x420
100defe04:     	mov	x1, x20
100defe08:     	ldr	x2, [sp, #0x328]
100defe0c:     	mov	x3, x19
100defe10:     	bl	0x100e34da0 <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB6_11Interpreter11run_add_imm>
100defe14:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100defe18:     	stp	x28, x26, [sp]
100defe1c:     	add	x0, sp, #0x420
100defe20:     	mov	x1, x20
100defe24:     	mov	x3, x19
100defe28:     	bl	0x100e367bc <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB6_11Interpreter17run_less_than_imm>
100defe2c:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100defe30:     	stp	x28, x26, [sp]
100defe34:     	add	x0, sp, #0x420
100defe38:     	mov	x1, x20
100defe3c:     	mov	x3, x19
100defe40:     	bl	0x100e36a18 <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB6_11Interpreter18run_increment_regs>
100defe44:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100defe48:     	stp	x28, x26, [sp]
100defe4c:     	add	x0, sp, #0x420
100defe50:     	mov	x1, x20
100defe54:     	ldr	x2, [sp, #0x328]
100defe58:     	mov	x3, x19
100defe5c:     	bl	0x100c372a8 <__RINvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB7_11Interpreter15run_numeric_immNvNtNtB7_6number7bitwise11bitwise_andEB7_>
100defe60:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100defe64:     	stp	x28, x26, [sp]
100defe68:     	add	x0, sp, #0x420
100defe6c:     	mov	x1, x20
100defe70:     	ldr	x2, [sp, #0x328]
100defe74:     	mov	x3, x19
100defe78:     	bl	0x100c37070 <__RINvMs_NtCsgTc3P8JF8aG_8otter_vm19arithmetic_dispatchNtB7_11Interpreter15run_numeric_immNvNtNtB7_6number5arith3subEB7_>
100defe7c:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100defe80:     	ldr	x9, [x25, #0x88]
100defe84:     	ldr	w8, [x27, #0xc]
100defe88:     	add	x8, x8, #0x3
100defe8c:     	cmp	x8, x9
100defe90:     	b.hs	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100defe94:     	ldr	x9, [x25, #0x80]
100defe98:     	add	x8, x9, x8, lsl #2
100defe9c:     	ldr	w0, [x8]
100defea0:     	bl	0x100e55040 <__RNvMsb_NtCslDZdWM0QmzH_14otter_bytecode13opcode_schemaNtB5_16MemberDefinition10from_imm32>
100defea4:     	and	w8, w0, #0xff
100defea8:     	cmp	w8, #0x2
100defeac:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100defeb0:     	ldp	w5, w6, [x27, #0xc]
100defeb4:     	ldr	w7, [x27, #0x14]
100defeb8:     	ldr	x4, [sp, #0x360]
100defebc:     	str	w0, [sp]
100defec0:     	add	x0, sp, #0x420
100defec4:     	mov	x1, x20
100defec8:     	ldr	x2, [sp, #0x308]
100defecc:     	mov	x3, x19
100defed0:     	bl	0x100d01348 <__RNvMNtCsgTc3P8JF8aG_8otter_vm15static_call_opsNtB4_11Interpreter22run_define_member_regs>
100defed4:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100defed8:     	ldr	x10, [x25, #0x88]
100defedc:     	add	x9, x19, #0x2
100defee0:     	cmp	x9, x10
100defee4:     	b.hs	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100defee8:     	ldr	x10, [x25, #0x80]
100defeec:     	add	x9, x10, x9, lsl #2
100defef0:     	ldr	w22, [x9]
100defef4:     	and	x9, x22, #0xff
100defef8:     	cmp	w22, #0x1ff
100defefc:     	ccmp	w9, #0x8, #0x2, ls
100deff00:     	b.hs	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100deff04:     	ldr	x21, [sp, #0x358]
100deff08:     	ldr	w10, [x21, #0x18]
100deff0c:     	cmp	w10, w8
100deff10:     	b.ls	0x100df1af0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7530>
100deff14:     	adrp	x10, 0x101c91000 <__RNvNtNtCs4Lsl61tBeg1_6brotli3enc9constants22kStaticCommandCodeBits+0x380>
100deff18:     	add	x10, x10, #0x9d8
100deff1c:     	ldrb	w23, [x10, x9]
100deff20:     	ldr	x9, [x21, #0x10]
100deff24:     	add	x8, x9, x8, lsl #3
100deff28:     	str	x8, [sp, #0x68]
100deff2c:     	ldr	x0, [x8]
100deff30:     	add	x1, x20, #0x430
100deff34:     	bl	0x100d256bc <__RNvMNtCsgTc3P8JF8aG_8otter_vm5valueNtB2_5Value21typeof_kind_with_heap>
100deff38:     	ldrh	w9, [x21, #0x8]
100deff3c:     	ldr	x8, [x21, #0x10]
100deff40:     	cmp	w9, w19, uxth
100deff44:     	ccmp	x8, #0x0, #0x4, hi
100deff48:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100deff4c:     	and	w9, w0, #0xff
100deff50:     	cmp	w22, #0xff
100deff54:     	cset	w10, hi
100deff58:     	cmp	w9, w23
100deff5c:     	cset	w9, eq
100deff60:     	eor	w9, w10, w9
100deff64:     	cmp	w9, #0x0
100deff68:     	mov	w9, #0x6                ; =6
100deff6c:     	cinc	x9, x9, ne
100deff70:     	and	x10, x19, #0xffff
100deff74:     	str	x9, [x8, x10, lsl #3]
100deff78:     	ldr	w8, [x21, #0x4]
100deff7c:     	cmn	w8, #0x1
100deff80:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100deff84:     	add	w8, w8, #0x1
100deff88:     	str	w8, [x21, #0x4]
100deff8c:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100deff90:     	add	x0, sp, #0x420
100deff94:     	mov	x1, x20
100deff98:     	ldr	x3, [sp, #0x308]
100deff9c:     	mov	x4, x25
100deffa0:     	mov	x5, x27
100deffa4:     	bl	0x100bfe6f4 <__RINvMNtCsgTc3P8JF8aG_8otter_vm10conversionNtB5_11Interpreter18drive_to_primitiveNtNtB5_10executable11OperandViewEB5_>
100deffa8:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100deffac:     	ldrh	w10, [x8, #0x8]
100deffb0:     	ldr	x9, [x8, #0x10]
100deffb4:     	cmp	w10, w21
100deffb8:     	ccmp	x9, #0x0, #0x4, hi
100deffbc:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100deffc0:     	mov	w10, #0xa               ; =10
100deffc4:     	str	x10, [x9, x21, lsl #3]
100deffc8:     	ldrh	w10, [x8, #0x8]
100deffcc:     	ldr	x9, [x8, #0x10]
100deffd0:     	cmp	w10, w19
100deffd4:     	ccmp	x9, #0x0, #0x4, hi
100deffd8:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100deffdc:     	mov	w10, #0x6               ; =6
100deffe0:     	str	x10, [x9, x19, lsl #3]
100deffe4:     	b	0x100def710 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5150>
100deffe8:     	ldr	x8, [x20, #0x77f0]
100deffec:     	ldapurb	w8, [x8, #0x18]
100defff0:     	ldr	x21, [sp, #0x328]
100defff4:     	cbnz	w8, 0x100df1334 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6d74>
100defff8:     	mov	w19, #0xff              ; =255
100defffc:     	add	x0, sp, #0x420
100df0000:     	mov	x1, x20
100df0004:     	mov	x2, x21
100df0008:     	mov	x3, x25
100df000c:     	mov	x4, x27
100df0010:     	bl	0x100ddf058 <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter17do_tail_call_exec>
100df0014:     	ldrb	w8, [sp, #0x420]
100df0018:     	cmp	w8, #0xff
100df001c:     	b.ne	0x100df0e20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6860>
100df0020:     	ldr	x8, [sp, #0x340]
100df0024:     	cbz	x8, 0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0028:     	ldr	x8, [sp, #0x328]
100df002c:     	ldr	x1, [x8, #0x128]
100df0030:     	ldrb	w0, [x8, #0x112]
100df0034:     	bl	0x100ddfe3c <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter22staged_bytecode_target>
100df0038:     	cmp	w0, #0x1
100df003c:     	ldr	x8, [sp, #0x38]
100df0040:     	b.ne	0x100df03b4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5df4>
100df0044:     	mov	w19, #0x0               ; =0
100df0048:     	bfi	x8, x1, #24, #40
100df004c:     	str	x8, [sp, #0x38]
100df0050:     	mov	x9, x8
100df0054:     	b	0x100df03c0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5e00>
100df0058:     	mov	w19, #0xff              ; =255
100df005c:     	ldr	x1, [sp, #0x328]
100df0060:     	add	x0, sp, #0x420
100df0064:     	mov	x2, x25
100df0068:     	mov	x3, x27
100df006c:     	bl	0x100dde78c <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter14do_call_spread>
100df0070:     	ldrb	w8, [sp, #0x420]
100df0074:     	cmp	w8, #0xff
100df0078:     	b.ne	0x100df0e20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6860>
100df007c:     	ldr	x8, [sp, #0x340]
100df0080:     	cbz	x8, 0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0084:     	ldr	x8, [sp, #0x328]
100df0088:     	ldr	x1, [x8, #0x128]
100df008c:     	ldrb	w0, [x8, #0x112]
100df0090:     	bl	0x100ddfe3c <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter22staged_bytecode_target>
100df0094:     	cmp	w0, #0x1
100df0098:     	ldr	x8, [sp, #0x48]
100df009c:     	b.ne	0x100df02f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5d38>
100df00a0:     	mov	w19, #0x0               ; =0
100df00a4:     	bfi	x8, x1, #24, #40
100df00a8:     	str	x8, [sp, #0x48]
100df00ac:     	mov	x9, x8
100df00b0:     	b	0x100df0304 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5d44>
100df00b4:     	mov	w24, #0x0               ; =0
100df00b8:     	mov	w23, #0xff              ; =255
100df00bc:     	ldr	x1, [sp, #0x328]
100df00c0:     	add	x0, sp, #0x420
100df00c4:     	mov	x2, x25
100df00c8:     	mov	x3, x27
100df00cc:     	bl	0x100ddf4c4 <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter19do_construct_spread>
100df00d0:     	ldrb	w8, [sp, #0x420]
100df00d4:     	cmp	w8, #0xff
100df00d8:     	b.ne	0x100df0e20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6860>
100df00dc:     	ldr	x8, [sp, #0x340]
100df00e0:     	cbz	x8, 0x100df0114 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5b54>
100df00e4:     	ldr	x8, [sp, #0x328]
100df00e8:     	ldr	x1, [x8, #0x128]
100df00ec:     	ldrb	w0, [x8, #0x112]
100df00f0:     	bl	0x100ddfe3c <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter22staged_bytecode_target>
100df00f4:     	and	w8, w0, w24
100df00f8:     	tbz	w8, #0x0, 0x100df0114 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5b54>
100df00fc:     	cmp	w26, w1
100df0100:     	b.ne	0x100df0114 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5b54>
100df0104:     	ldr	w1, [x27]
100df0108:     	lsl	x2, x26, #32
100df010c:     	mov	x0, x25
100df0110:     	bl	0x100cf6f38 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter29record_ordinary_call_feedback>
100df0114:     	cmp	w23, #0xff
100df0118:     	b.eq	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df011c:     	ldr	w1, [x27]
100df0120:     	mov	w8, w23
100df0124:     	ldr	x9, [sp, #0x298]
100df0128:     	orr	x2, x8, x9, lsl #8
100df012c:     	mov	x0, x25
100df0130:     	bl	0x100cf6f38 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter29record_ordinary_call_feedback>
100df0134:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0138:     	mov	w23, #0x0               ; =0
100df013c:     	mov	w19, #0xff              ; =255
100df0140:     	b	0x100df0684 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x60c4>
100df0144:     	mov	w5, #0x0                ; =0
100df0148:     	ldr	x3, [sp, #0x328]
100df014c:     	b	0x100df05e0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6020>
100df0150:     	mov	w24, #0x0               ; =0
100df0154:     	mov	w23, #0xff              ; =255
100df0158:     	ldr	x1, [sp, #0x328]
100df015c:     	add	x0, sp, #0x420
100df0160:     	mov	x2, x25
100df0164:     	mov	x3, x27
100df0168:     	bl	0x100de07c0 <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter25do_super_construct_spread>
100df016c:     	ldrb	w8, [sp, #0x420]
100df0170:     	cmp	w8, #0xff
100df0174:     	b.ne	0x100df0e20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6860>
100df0178:     	ldr	x8, [sp, #0x340]
100df017c:     	cbz	x8, 0x100df01b0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5bf0>
100df0180:     	ldr	x8, [sp, #0x328]
100df0184:     	ldr	x1, [x8, #0x128]
100df0188:     	ldrb	w0, [x8, #0x112]
100df018c:     	bl	0x100ddfe3c <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter22staged_bytecode_target>
100df0190:     	and	w8, w0, w24
100df0194:     	tbz	w8, #0x0, 0x100df01b0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5bf0>
100df0198:     	cmp	w26, w1
100df019c:     	b.ne	0x100df01b0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5bf0>
100df01a0:     	ldr	w1, [x27]
100df01a4:     	lsl	x2, x26, #32
100df01a8:     	mov	x0, x25
100df01ac:     	bl	0x100cf6f38 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter29record_ordinary_call_feedback>
100df01b0:     	cmp	w23, #0xff
100df01b4:     	b.eq	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df01b8:     	ldr	w1, [x27]
100df01bc:     	mov	w8, w23
100df01c0:     	ldr	x9, [sp, #0x2a0]
100df01c4:     	orr	x2, x8, x9, lsl #8
100df01c8:     	mov	x0, x25
100df01cc:     	bl	0x100cf6f38 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter29record_ordinary_call_feedback>
100df01d0:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df01d4:     	mov	w24, #0x0               ; =0
100df01d8:     	mov	w23, #0xff              ; =255
100df01dc:     	b	0x100df0708 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6148>
100df01e0:     	add	x0, sp, #0x420
100df01e4:     	mov	x1, x20
100df01e8:     	ldr	x2, [sp, #0x328]
100df01ec:     	mov	x3, x19
100df01f0:     	mov	x4, x25
100df01f4:     	mov	x5, x27
100df01f8:     	bl	0x100de1940 <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter30do_call_forward_arguments_exec>
100df01fc:     	ldr	x8, [sp, #0x420]
100df0200:     	cmp	x8, #0x2
100df0204:     	b.eq	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0208:     	b	0x100df18fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x733c>
100df020c:     	ldr	w24, [x27, #0x4]
100df0210:     	mov	w8, #-0x1               ; =-1
100df0214:     	cmp	x24, x8
100df0218:     	cset	w19, ne
100df021c:     	b	0x100df07cc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x620c>
100df0220:     	ubfx	x8, x8, #52, #11
100df0224:     	cmp	x8, #0x7fe
100df0228:     	b.hi	0x100df0254 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5c94>
100df022c:     	frintz	d1, d0
100df0230:     	fcmp	d0, d1
100df0234:     	mov	x8, #-0x3e20000000000000 ; =-4476578029606273024
100df0238:     	fmov	d1, x8
100df023c:     	fccmp	d0, d1, #0x8, eq
100df0240:     	mov	x8, #0xffffffc00000     ; =281474972516352
100df0244:     	movk	x8, #0x41df, lsl #48
100df0248:     	fmov	d1, x8
100df024c:     	fccmp	d0, d1, #0x2, ge
100df0250:     	b.ls	0x100df0608 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6048>
100df0254:     	ldr	x8, [sp, #0x358]
100df0258:     	fcvtzs	w9, d0
100df025c:     	scvtf	d1, w9
100df0260:     	fcmp	d0, d1
100df0264:     	b.ne	0x100df0398 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5dd8>
100df0268:     	cbnz	w9, 0x100df0610 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6050>
100df026c:     	fmov	x10, d0
100df0270:     	tbnz	x10, #0x3f, 0x100df039c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5ddc>
100df0274:     	b	0x100df0610 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6050>
100df0278:     	mov	w0, #0x1                ; =1
100df027c:     	b	0x100df02b4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5cf4>
100df0280:     	mov	x8, #0x2                ; =2
100df0284:     	movk	x8, #0xfffe, lsl #48
100df0288:     	and	x8, x23, x8
100df028c:     	cmp	x23, #0x0
100df0290:     	ccmp	x8, #0x0, #0x0, ne
100df0294:     	b.eq	0x100df02ac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5cec>
100df0298:     	mov	x0, x23
100df029c:     	bl	0x100d255a8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm5valueNtB2_5Value15to_boolean_pure>
100df02a0:     	and	w0, w0, #0xff
100df02a4:     	cmp	w0, #0x2
100df02a8:     	b.ne	0x100df02b4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5cf4>
100df02ac:     	mov	x0, x23
100df02b0:     	bl	0x100d253e8 <__RNvMNtCsgTc3P8JF8aG_8otter_vm5valueNtB2_5Value15to_boolean_cell>
100df02b4:     	ldrh	w9, [x19, #0x8]
100df02b8:     	ldr	x8, [x19, #0x10]
100df02bc:     	cmp	w9, w21
100df02c0:     	ccmp	x8, #0x0, #0x4, hi
100df02c4:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100df02c8:     	tst	w0, #0x1
100df02cc:     	mov	w9, #0x6                ; =6
100df02d0:     	cinc	x9, x9, eq
100df02d4:     	str	x9, [x8, x21, lsl #3]
100df02d8:     	b	0x100df05c4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6004>
100df02dc:     	ldr	x9, [x25, #0x88]
100df02e0:     	ldr	w8, [x27, #0xc]
100df02e4:     	cmp	x9, x8
100df02e8:     	b.ls	0x100df05dc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x601c>
100df02ec:     	ldr	x9, [x25, #0x80]
100df02f0:     	ldr	w5, [x9, x8, lsl #2]
100df02f4:     	b	0x100df05e0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6020>
100df02f8:     	cmp	w19, #0xff
100df02fc:     	b.eq	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0300:     	ldr	x9, [sp, #0x1b0]
100df0304:     	ldr	w1, [x27]
100df0308:     	mov	w8, w19
100df030c:     	str	x9, [sp, #0x1b0]
100df0310:     	orr	x2, x8, x9, lsl #8
100df0314:     	mov	x0, x25
100df0318:     	bl	0x100cf6f38 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter29record_ordinary_call_feedback>
100df031c:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0320:     	str	x22, [sp, #0x2b8]
100df0324:     	mov	x19, x21
100df0328:     	ldr	x8, [sp, #0x278]
100df032c:     	str	x8, [sp, #0x2c0]
100df0330:     	cmp	w21, #0xff
100df0334:     	b.ne	0x100df0348 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5d88>
100df0338:     	str	x22, [sp, #0x2b8]
100df033c:     	ldr	x8, [sp, #0x278]
100df0340:     	str	x8, [sp, #0x2c0]
100df0344:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0348:     	ldr	w1, [x27]
100df034c:     	ldr	x8, [sp, #0x2c0]
100df0350:     	lsl	x8, x8, #8
100df0354:     	ldr	x9, [sp, #0x2b8]
100df0358:     	orr	x8, x8, x9, lsl #32
100df035c:     	mov	w9, w19
100df0360:     	orr	x2, x8, x9
100df0364:     	mov	x0, x25
100df0368:     	bl	0x100cf6f38 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter29record_ordinary_call_feedback>
100df036c:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0370:     	mvn	w8, w23
100df0374:     	tst	x8, #0xff
100df0378:     	b.eq	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df037c:     	lsr	x8, x23, #8
100df0380:     	ldr	w1, [x27]
100df0384:     	bfi	x23, x8, #8, #56
100df0388:     	mov	x0, x25
100df038c:     	mov	x2, x23
100df0390:     	bl	0x100cf6f38 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter29record_ordinary_call_feedback>
100df0394:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0398:     	fmov	x10, d0
100df039c:     	mov	x9, #0x2000000000000    ; =562949953421312
100df03a0:     	add	x9, x10, x9
100df03a4:     	fcmp	d0, d0
100df03a8:     	mov	x10, #0x7ffa000000000000 ; =9221683186994511872
100df03ac:     	csel	x9, x10, x9, vs
100df03b0:     	b	0x100df0614 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6054>
100df03b4:     	cmp	w19, #0xff
100df03b8:     	b.eq	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df03bc:     	ldr	x9, [sp, #0x1a8]
100df03c0:     	ldr	w1, [x27]
100df03c4:     	mov	w8, w19
100df03c8:     	str	x9, [sp, #0x1a8]
100df03cc:     	orr	x2, x8, x9, lsl #8
100df03d0:     	mov	x0, x25
100df03d4:     	bl	0x100cf6f38 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter29record_ordinary_call_feedback>
100df03d8:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df03dc:     	ldr	x9, [x25, #0x88]
100df03e0:     	add	x8, x5, #0x1
100df03e4:     	cmp	x8, x9
100df03e8:     	b.hs	0x100df03fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5e3c>
100df03ec:     	ldr	x9, [x25, #0x80]
100df03f0:     	ldr	w9, [x9, x8, lsl #2]
100df03f4:     	mov	w8, #0x1                ; =1
100df03f8:     	b	0x100df0400 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5e40>
100df03fc:     	mov	w8, #0x0                ; =0
100df0400:     	cmp	w9, #0x0
100df0404:     	csel	w6, wzr, w8, eq
100df0408:     	ldr	x4, [sp, #0x358]
100df040c:     	ldr	x2, [x19]
100df0410:     	ldr	w3, [x19, #0x10]
100df0414:     	add	x0, sp, #0x420
100df0418:     	mov	x1, x20
100df041c:     	bl	0x100cc8328 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10global_opsNtB4_11Interpreter26run_declare_global_var_reg>
100df0420:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100df0424:     	ldr	x9, [x25, #0x88]
100df0428:     	add	x8, x5, #0x2
100df042c:     	cmp	x8, x9
100df0430:     	b.hs	0x100df0444 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5e84>
100df0434:     	ldr	x9, [x25, #0x80]
100df0438:     	ldr	w9, [x9, x8, lsl #2]
100df043c:     	mov	w8, #0x1                ; =1
100df0440:     	b	0x100df0448 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5e88>
100df0444:     	mov	w8, #0x0                ; =0
100df0448:     	ldr	x3, [sp, #0x328]
100df044c:     	ldr	w6, [x27, #0x10]
100df0450:     	cmp	w9, #0x0
100df0454:     	csel	w7, wzr, w8, eq
100df0458:     	ldr	x4, [sp, #0x360]
100df045c:     	add	x0, sp, #0x420
100df0460:     	mov	x1, x20
100df0464:     	mov	x2, x19
100df0468:     	bl	0x100ccad34 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10global_opsNtB4_11Interpreter28run_store_global_binding_reg>
100df046c:     	b	0x100df0c38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6678>
100df0470:     	ldr	x9, [x25, #0x88]
100df0474:     	add	x8, x5, #0x1
100df0478:     	cmp	x8, x9
100df047c:     	b.hs	0x100df0490 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5ed0>
100df0480:     	ldr	x9, [x25, #0x80]
100df0484:     	ldr	w9, [x9, x8, lsl #2]
100df0488:     	mov	w8, #0x1                ; =1
100df048c:     	b	0x100df0494 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5ed4>
100df0490:     	mov	w8, #0x0                ; =0
100df0494:     	cmp	w9, #0x0
100df0498:     	csel	w6, wzr, w8, eq
100df049c:     	ldr	x4, [sp, #0x358]
100df04a0:     	ldr	x2, [x19]
100df04a4:     	ldr	w3, [x19, #0x10]
100df04a8:     	add	x0, sp, #0x420
100df04ac:     	mov	x1, x20
100df04b0:     	bl	0x100cc82b0 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10global_opsNtB4_11Interpreter26run_declare_global_lex_reg>
100df04b4:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100df04b8:     	ldr	x9, [x25, #0x88]
100df04bc:     	add	x8, x5, #0x1
100df04c0:     	cmp	x8, x9
100df04c4:     	b.hs	0x100df04d4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5f14>
100df04c8:     	ldr	x9, [x25, #0x80]
100df04cc:     	ldr	w6, [x9, x8, lsl #2]
100df04d0:     	b	0x100df04d8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5f18>
100df04d4:     	mov	w6, #0x0                ; =0
100df04d8:     	ldr	x4, [sp, #0x358]
100df04dc:     	ldr	x2, [x19]
100df04e0:     	ldr	w3, [x19, #0x10]
100df04e4:     	add	x0, sp, #0x420
100df04e8:     	mov	x1, x20
100df04ec:     	bl	0x100ccb0c4 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10global_opsNtB4_11Interpreter28run_validate_global_decl_reg>
100df04f0:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100df04f4:     	ldr	x9, [x25, #0x88]
100df04f8:     	add	x8, x5, #0x2
100df04fc:     	cmp	x8, x9
100df0500:     	b.hs	0x100df0514 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5f54>
100df0504:     	ldr	x9, [x25, #0x80]
100df0508:     	ldr	w9, [x9, x8, lsl #2]
100df050c:     	mov	w8, #0x1                ; =1
100df0510:     	b	0x100df0518 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5f58>
100df0514:     	mov	w8, #0x0                ; =0
100df0518:     	ldr	w6, [x27, #0x10]
100df051c:     	cmp	w9, #0x0
100df0520:     	csel	w7, wzr, w8, eq
100df0524:     	ldr	x4, [sp, #0x358]
100df0528:     	ldr	x2, [x19]
100df052c:     	ldr	w3, [x19, #0x10]
100df0530:     	add	x0, sp, #0x420
100df0534:     	mov	x1, x20
100df0538:     	bl	0x100ccba70 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10global_opsNtB4_11Interpreter30run_define_global_function_reg>
100df053c:     	b	0x100df05f8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6038>
100df0540:     	lsr	x2, x0, #16
100df0544:     	ldr	x0, [x1, #0x180]
100df0548:     	ldr	x1, [sp, #0x360]
100df054c:     	bl	0x100ca9c7c <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickss2_0Bb_>
100df0550:     	cmp	x0, #0x1
100df0554:     	b.ne	0x100df0680 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x60c0>
100df0558:     	mov	x0, x1
100df055c:     	bl	0x100caa784 <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickssh_0Bb_>
100df0560:     	mov	x23, x0
100df0564:     	mov	x24, x1
100df0568:     	b	0x100df0684 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x60c4>
100df056c:     	lsr	x2, x0, #16
100df0570:     	ldr	x0, [x1, #0x180]
100df0574:     	ldr	x1, [sp, #0x360]
100df0578:     	bl	0x100ca9c7c <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickss2_0Bb_>
100df057c:     	cmp	x0, #0x1
100df0580:     	b.ne	0x100df0700 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6140>
100df0584:     	mov	x0, x1
100df0588:     	bl	0x100caa784 <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickssh_0Bb_>
100df058c:     	mov	x24, x0
100df0590:     	mov	x26, x1
100df0594:     	str	xzr, [sp, #0x290]
100df0598:     	b	0x100df0708 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6148>
100df059c:     	mov	x21, #0x0               ; =0
100df05a0:     	b	0x100df079c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x61dc>
100df05a4:     	mov	w23, #0x6               ; =6
100df05a8:     	ldrh	w9, [x19, #0x8]
100df05ac:     	ldr	x8, [x19, #0x10]
100df05b0:     	cmp	w9, w21
100df05b4:     	ccmp	x8, #0x0, #0x4, hi
100df05b8:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100df05bc:     	str	x22, [sp, #0xf0]
100df05c0:     	str	x23, [x8, x21, lsl #3]
100df05c4:     	ldr	w8, [x19, #0x4]
100df05c8:     	cmn	w8, #0x1
100df05cc:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100df05d0:     	add	w8, w8, #0x1
100df05d4:     	str	w8, [x19, #0x4]
100df05d8:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df05dc:     	mov	w5, #0x0                ; =0
100df05e0:     	ldr	w6, [x27, #0x10]
100df05e4:     	ldr	x4, [sp, #0x360]
100df05e8:     	add	x0, sp, #0x420
100df05ec:     	mov	x1, x20
100df05f0:     	mov	x2, x19
100df05f4:     	bl	0x100d29b6c <__RNvMNtCsgTc3P8JF8aG_8otter_vm9class_opsNtB4_11Interpreter19run_class_check_reg>
100df05f8:     	ldrb	w8, [sp, #0x420]
100df05fc:     	cmp	w8, #0xff
100df0600:     	b.eq	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0604:     	b	0x100df0e20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6860>
100df0608:     	fcvtzs	w9, d0
100df060c:     	ldr	x8, [sp, #0x358]
100df0610:     	orr	x9, x9, #0xfffe000000000000
100df0614:     	ldrh	w10, [x27, #0xc]
100df0618:     	ldrh	w12, [x8, #0x8]
100df061c:     	ldr	x11, [x8, #0x10]
100df0620:     	cmp	w12, w10
100df0624:     	ccmp	x11, #0x0, #0x4, hi
100df0628:     	b.eq	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100df062c:     	str	x9, [x11, x10, lsl #3]
100df0630:     	b	0x100def710 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5150>
100df0634:     	adrp	x8, 0x1034f4000 <_global_added_lock+0x68>
100df0638:     	add	x8, x8, #0xef8
100df063c:     	ldr	x8, [x8]
100df0640:     	mov	w9, #-0x3               ; =-3
100df0644:     	and	x9, x28, x9
100df0648:     	ldrb	w8, [x8, x9]
100df064c:     	cmp	w8, #0x11
100df0650:     	b.ne	0x100deefc0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4a00>
100df0654:     	mov	x0, x28
100df0658:     	bl	0x100ca4ec8 <__RNCNvMNtCsgTc3P8JF8aG_8otter_vm14jit_delete_opsNtB6_11Interpreter21jit_runtime_delete_op0B6_>
100df065c:     	tbz	w0, #0x0, 0x100deefc0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4a00>
100df0660:     	cmp	x19, #0x4
100df0664:     	b.ne	0x100df0bb8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x65f8>
100df0668:     	ldr	w8, [x26]
100df066c:     	mov	w9, #0x6874             ; =26740
100df0670:     	movk	w9, #0x6e65, lsl #16
100df0674:     	cmp	w8, w9
100df0678:     	cset	w5, ne
100df067c:     	b	0x100df0bbc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x65fc>
100df0680:     	mov	w23, #0x0               ; =0
100df0684:     	ldr	x1, [sp, #0x328]
100df0688:     	add	x0, sp, #0x420
100df068c:     	mov	x2, x25
100df0690:     	mov	x3, x27
100df0694:     	bl	0x100ddec5c <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter17do_construct_exec>
100df0698:     	ldrb	w8, [sp, #0x420]
100df069c:     	cmp	w8, #0xff
100df06a0:     	b.ne	0x100df0e20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6860>
100df06a4:     	ldr	x8, [sp, #0x340]
100df06a8:     	cbz	x8, 0x100df06dc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x611c>
100df06ac:     	ldr	x8, [sp, #0x328]
100df06b0:     	ldr	x1, [x8, #0x128]
100df06b4:     	ldrb	w0, [x8, #0x112]
100df06b8:     	bl	0x100ddfe3c <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter22staged_bytecode_target>
100df06bc:     	and	w8, w0, w23
100df06c0:     	tbz	w8, #0x0, 0x100df06dc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x611c>
100df06c4:     	cmp	w24, w1
100df06c8:     	b.ne	0x100df06dc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x611c>
100df06cc:     	ldr	w1, [x27]
100df06d0:     	lsl	x2, x24, #32
100df06d4:     	mov	x0, x25
100df06d8:     	bl	0x100cf6f38 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter29record_ordinary_call_feedback>
100df06dc:     	cmp	w19, #0xff
100df06e0:     	b.eq	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df06e4:     	ldr	w1, [x27]
100df06e8:     	mov	w8, w19
100df06ec:     	ldr	x9, [sp, #0x2a8]
100df06f0:     	orr	x2, x8, x9, lsl #8
100df06f4:     	mov	x0, x25
100df06f8:     	bl	0x100cf6f38 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter29record_ordinary_call_feedback>
100df06fc:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0700:     	str	xzr, [sp, #0x290]
100df0704:     	mov	w24, #0x0               ; =0
100df0708:     	ldr	x1, [sp, #0x328]
100df070c:     	add	x0, sp, #0x420
100df0710:     	mov	x2, x25
100df0714:     	mov	x3, x27
100df0718:     	bl	0x100ddfef4 <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter23do_super_construct_exec>
100df071c:     	ldrb	w8, [sp, #0x420]
100df0720:     	cmp	w8, #0xff
100df0724:     	b.ne	0x100df0e20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6860>
100df0728:     	ldr	x8, [sp, #0x340]
100df072c:     	cbz	x8, 0x100df0760 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x61a0>
100df0730:     	ldr	x8, [sp, #0x328]
100df0734:     	ldr	x1, [x8, #0x128]
100df0738:     	ldrb	w0, [x8, #0x112]
100df073c:     	bl	0x100ddfe3c <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter22staged_bytecode_target>
100df0740:     	and	w8, w0, w24
100df0744:     	tbz	w8, #0x0, 0x100df0760 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x61a0>
100df0748:     	cmp	w26, w1
100df074c:     	b.ne	0x100df0760 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x61a0>
100df0750:     	ldr	w1, [x27]
100df0754:     	lsl	x2, x26, #32
100df0758:     	mov	x0, x25
100df075c:     	bl	0x100cf6f38 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter29record_ordinary_call_feedback>
100df0760:     	cmp	w23, #0xff
100df0764:     	b.eq	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0768:     	ldr	w1, [x27]
100df076c:     	mov	w8, w23
100df0770:     	ldr	x9, [sp, #0x290]
100df0774:     	orr	x2, x8, x9, lsl #8
100df0778:     	mov	x0, x25
100df077c:     	bl	0x100cf6f38 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter29record_ordinary_call_feedback>
100df0780:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0784:     	ldr	x8, [x20, #0x7430]
100df0788:     	add	x9, x24, x24, lsl #2
100df078c:     	lsl	x9, x9, #3
100df0790:     	ldr	w8, [x8, x9]
100df0794:     	cmp	w8, #0x3
100df0798:     	cset	w21, ne
100df079c:     	mov	x0, x25
100df07a0:     	mov	x1, x27
100df07a4:     	bl	0x100caa23c <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickss7_0Bb_>
100df07a8:     	tbz	w0, #0x0, 0x100df07cc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x620c>
100df07ac:     	mov	x2, x1
100df07b0:     	ldr	x8, [sp, #0x328]
100df07b4:     	ldr	x0, [x8, #0x180]
100df07b8:     	ldr	x1, [sp, #0x360]
100df07bc:     	bl	0x100caa2e0 <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickss8_0Bb_>
100df07c0:     	mov	x26, x1
100df07c4:     	and	x21, x0, x21
100df07c8:     	b	0x100df07d0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6210>
100df07cc:     	mov	x21, #0x0               ; =0
100df07d0:     	stp	x21, x26, [x29, #-0xa8]
100df07d4:     	mov	x0, x25
100df07d8:     	mov	x1, x27
100df07dc:     	mov	w2, #0x2                ; =2
100df07e0:     	bl	0x100db4240 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock7operand>
100df07e4:     	and	x22, x0, #0xffff
100df07e8:     	lsr	x23, x0, #32
100df07ec:     	cmp	w19, #0x0
100df07f0:     	ccmp	w21, #0x0, #0x4, ne
100df07f4:     	ccmp	x22, #0x1, #0x0, ne
100df07f8:     	b.ne	0x100df087c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x62bc>
100df07fc:     	mov	x0, x25
100df0800:     	mov	x1, x27
100df0804:     	mov	w2, #0x3                ; =3
100df0808:     	bl	0x100db4240 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock7operand>
100df080c:     	and	x8, x0, #0xffff
100df0810:     	cmp	x8, #0x1
100df0814:     	b.ne	0x100df087c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x62bc>
100df0818:     	mov	x28, x0
100df081c:     	ldr	w3, [x27]
100df0820:     	ldr	w2, [sp, #0x36c]
100df0824:     	ldr	x8, [sp, #0x308]
100df0828:     	ldr	x0, [x8]
100df082c:     	ldr	w1, [x8, #0x10]
100df0830:     	mov	w4, #0x0                ; =0
100df0834:     	bl	0x100dbe7f8 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm17execution_contextNtB5_16ExecutionContext22property_feedback_slot>
100df0838:     	mov	x6, x0
100df083c:     	mov	x7, x1
100df0840:     	lsr	x8, x28, #32
100df0844:     	ldr	w3, [sp, #0x36c]
100df0848:     	ldr	x9, [sp, #0x308]
100df084c:     	ldr	x1, [x9]
100df0850:     	ldr	w2, [x9, #0x10]
100df0854:     	stp	x26, x8, [sp]
100df0858:     	mov	x0, x20
100df085c:     	mov	x4, x23
100df0860:     	mov	x5, x24
100df0864:     	bl	0x100d18014 <__RNvMNtCsgTc3P8JF8aG_8otter_vm19jit_spread_call_opsNtB4_11Interpreter20recorded_method_call>
100df0868:     	cmp	x0, #0x1
100df086c:     	b.ne	0x100df087c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x62bc>
100df0870:     	mov	x21, #0x0               ; =0
100df0874:     	stur	xzr, [x29, #-0xa8]
100df0878:     	b	0x100df08bc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x62fc>
100df087c:     	cmp	x22, #0x1
100df0880:     	b.ne	0x100df08bc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x62fc>
100df0884:     	eor	w8, w21, #0x1
100df0888:     	tbnz	w8, #0x0, 0x100df08bc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x62fc>
100df088c:     	ldr	w4, [sp, #0x36c]
100df0890:     	ldr	x8, [sp, #0x308]
100df0894:     	ldr	x2, [x8]
100df0898:     	ldr	w3, [x8, #0x10]
100df089c:     	add	x0, sp, #0x3f8
100df08a0:     	sub	x8, x29, #0xa8
100df08a4:     	add	x6, x8, #0x8
100df08a8:     	mov	x1, x20
100df08ac:     	mov	x5, x23
100df08b0:     	bl	0x100d75f3c <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp11jit_compileNtB6_11Interpreter24method_site_for_receiver>
100df08b4:     	ldur	x21, [x29, #-0xa8]
100df08b8:     	b	0x100df08c0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6300>
100df08bc:     	str	xzr, [sp, #0x3f8]
100df08c0:     	mov	w19, w19
100df08c4:     	tbz	w21, #0x0, 0x100df0920 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6360>
100df08c8:     	ldur	x9, [x29, #-0xa0]
100df08cc:     	mov	w10, #-0x3              ; =-3
100df08d0:     	ldr	x8, [sp, #0x3f8]
100df08d4:     	ands	x10, x9, x10
100df08d8:     	b.eq	0x100df0924 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6364>
100df08dc:     	mov	x11, #0x2               ; =2
100df08e0:     	movk	x11, #0xfffe, lsl #48
100df08e4:     	and	x9, x9, x11
100df08e8:     	cbnz	x9, 0x100df0924 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6364>
100df08ec:     	adrp	x9, 0x1034f4000 <_global_added_lock+0x68>
100df08f0:     	add	x9, x9, #0xef8
100df08f4:     	ldr	x9, [x9]
100df08f8:     	ldrb	w9, [x9, x10]
100df08fc:     	cmp	x19, #0x0
100df0900:     	ccmp	w9, #0x11, #0x0, ne
100df0904:     	ccmp	x8, #0x0, #0x0, eq
100df0908:     	b.ne	0x100df0924 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6364>
100df090c:     	mov	x0, x20
100df0910:     	mov	x1, x24
100df0914:     	bl	0x100e50b44 <__RNvMs_NtNtCsgTc3P8JF8aG_8otter_vm6interp8feedbackNtB8_11Interpreter29saturate_method_site_feedback>
100df0918:     	mov	x8, #0x0                ; =0
100df091c:     	b	0x100df0924 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6364>
100df0920:     	ldr	x8, [sp, #0x3f8]
100df0924:     	mov	w26, #0x0               ; =0
100df0928:     	cmp	x22, #0x1
100df092c:     	b.ne	0x100df09a0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x63e0>
100df0930:     	and	x8, x8, x21
100df0934:     	ldr	x21, [sp, #0x328]
100df0938:     	cbz	x8, 0x100df09ac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x63ec>
100df093c:     	ldur	x8, [x29, #-0xa0]
100df0940:     	stur	x8, [x29, #-0x78]
100df0944:     	stur	w23, [x29, #-0x80]
100df0948:     	mov	x0, x25
100df094c:     	mov	x1, x27
100df0950:     	mov	w2, #0x3                ; =3
100df0954:     	bl	0x100db4240 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10executableNtB5_9CodeBlock7operand>
100df0958:     	and	x8, x0, #0xffff
100df095c:     	cmp	x8, #0x1
100df0960:     	b.ne	0x100df09a8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x63e8>
100df0964:     	lsr	x1, x0, #32
100df0968:     	str	x20, [sp, #0x420]
100df096c:     	ldr	x8, [sp, #0x308]
100df0970:     	str	x8, [sp, #0x428]
100df0974:     	add	x8, sp, #0x36c
100df0978:     	sub	x9, x29, #0x80
100df097c:     	str	x8, [sp, #0x430]
100df0980:     	str	x9, [sp, #0x438]
100df0984:     	sub	x8, x29, #0x78
100df0988:     	str	x8, [sp, #0x440]
100df098c:     	add	x0, sp, #0x420
100df0990:     	bl	0x100caa3ec <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickssb_0Bb_>
100df0994:     	mov	x26, x0
100df0998:     	mov	x28, x1
100df099c:     	b	0x100df09ac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x63ec>
100df09a0:     	ldr	x21, [sp, #0x328]
100df09a4:     	b	0x100df09ac <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x63ec>
100df09a8:     	mov	w26, #0x0               ; =0
100df09ac:     	add	x0, sp, #0x420
100df09b0:     	mov	x1, x20
100df09b4:     	mov	x2, x21
100df09b8:     	ldr	x3, [sp, #0x308]
100df09bc:     	mov	x4, x25
100df09c0:     	mov	x5, x27
100df09c4:     	bl	0x100dd0cec <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm10method_opsNtB7_11Interpreter26do_call_method_value_inner>
100df09c8:     	ldr	x8, [sp, #0x420]
100df09cc:     	cmp	x8, #0x2
100df09d0:     	b.ne	0x100df1610 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7050>
100df09d4:     	ldr	x8, [sp, #0x340]
100df09d8:     	cbz	x8, 0x100df0b5c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x659c>
100df09dc:     	ldrb	w8, [x21, #0x112]
100df09e0:     	cmp	w8, #0xff
100df09e4:     	b.eq	0x100df0a08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6448>
100df09e8:     	ldp	x0, x1, [x21, #0x128]
100df09ec:     	bl	0x100caa550 <__RNCNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtBb_11Interpreter14dispatch_tickssd_0Bb_>
100df09f0:     	mov	x2, x0
100df09f4:     	mvn	w8, w2
100df09f8:     	tst	x8, #0xff
100df09fc:     	b.ne	0x100df0b50 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6590>
100df0a00:     	ldrb	w0, [x21, #0x112]
100df0a04:     	b	0x100df0a0c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x644c>
100df0a08:     	mov	w0, #0xff               ; =255
100df0a0c:     	ldr	x1, [x21, #0x128]
100df0a10:     	bl	0x100ddfe3c <__RNvMs1_NtCsgTc3P8JF8aG_8otter_vm8call_opsNtB7_11Interpreter22staged_bytecode_target>
100df0a14:     	tbz	w0, #0x0, 0x100df0a98 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x64d8>
100df0a18:     	ldr	x8, [sp, #0x418]
100df0a1c:     	add	x9, sp, #0x420
100df0a20:     	stur	x8, [x9, #0x30]
100df0a24:     	add	x8, sp, #0x309
100df0a28:     	ldur	q0, [x8, #0xff]
100df0a2c:     	add	x8, sp, #0x2f9
100df0a30:     	ldur	q1, [x8, #0xff]
100df0a34:     	stp	q1, q0, [x9, #0x10]
100df0a38:     	add	x10, sp, #0x420
100df0a3c:     	str	x19, [sp, #0x420]
100df0a40:     	str	x24, [sp, #0x428]
100df0a44:     	ldr	w8, [sp, #0x430]
100df0a48:     	cbz	x19, 0x100df0b24 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6564>
100df0a4c:     	tbz	w8, #0x0, 0x100df0b24 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6564>
100df0a50:     	mov	x2, x1
100df0a54:     	add	x8, sp, #0x3f8
100df0a58:     	add	x3, x8, #0x8
100df0a5c:     	mov	x0, x20
100df0a60:     	mov	x1, x24
100df0a64:     	bl	0x100e50728 <__RNvMs_NtNtCsgTc3P8JF8aG_8otter_vm6interp8feedbackNtB8_11Interpreter29record_method_target_feedback>
100df0a68:     	cbz	w0, 0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0a6c:     	ldr	w8, [x25, #0xf0]
100df0a70:     	cmn	w8, #0x1
100df0a74:     	b.eq	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0a78:     	add	w9, w8, #0x1
100df0a7c:     	add	x10, x25, #0xf0
100df0a80:     	mov	x11, x8
100df0a84:     	casl	w11, w9, [x10]
100df0a88:     	cmp	w11, w8
100df0a8c:     	mov	x8, x11
100df0a90:     	b.ne	0x100df0a70 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x64b0>
100df0a94:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0a98:     	ldr	x8, [sp, #0x418]
100df0a9c:     	add	x9, sp, #0x420
100df0aa0:     	stur	x8, [x9, #0x30]
100df0aa4:     	add	x8, sp, #0x309
100df0aa8:     	ldur	q0, [x8, #0xff]
100df0aac:     	add	x8, sp, #0x2f9
100df0ab0:     	ldur	q1, [x8, #0xff]
100df0ab4:     	stp	q1, q0, [x9, #0x10]
100df0ab8:     	add	x10, sp, #0x420
100df0abc:     	str	x19, [sp, #0x420]
100df0ac0:     	str	x24, [sp, #0x428]
100df0ac4:     	str	w26, [sp, #0x458]
100df0ac8:     	str	w28, [sp, #0x45c]
100df0acc:     	ldr	w8, [sp, #0x430]
100df0ad0:     	cbz	x19, 0x100df0b24 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6564>
100df0ad4:     	cbz	w8, 0x100df0b24 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6564>
100df0ad8:     	tbz	w26, #0x0, 0x100df0b24 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6564>
100df0adc:     	add	x8, sp, #0x3f8
100df0ae0:     	add	x3, x8, #0x8
100df0ae4:     	mov	x0, x20
100df0ae8:     	mov	x1, x24
100df0aec:     	mov	x2, x28
100df0af0:     	bl	0x100e50c04 <__RNvMs_NtNtCsgTc3P8JF8aG_8otter_vm6interp8feedbackNtB8_11Interpreter34record_method_native_leaf_feedback>
100df0af4:     	cbz	w0, 0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0af8:     	ldr	w8, [x25, #0xf0]
100df0afc:     	cmn	w8, #0x1
100df0b00:     	b.eq	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0b04:     	add	w9, w8, #0x1
100df0b08:     	add	x10, x25, #0xf0
100df0b0c:     	mov	x11, x8
100df0b10:     	casl	w11, w9, [x10]
100df0b14:     	cmp	w11, w8
100df0b18:     	mov	x8, x11
100df0b1c:     	b.ne	0x100df0afc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x653c>
100df0b20:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0b24:     	tbz	w8, #0x0, 0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0b28:     	ldr	x8, [sp, #0x438]
100df0b2c:     	cbz	x8, 0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0b30:     	mov	x9, #-0x1               ; =-1
100df0b34:     	ldaddl	x9, x8, [x8]
100df0b38:     	cmp	x8, #0x1
100df0b3c:     	b.ne	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0b40:     	dmb	ishld
100df0b44:     	add	x0, x10, #0x18
100df0b48:     	bl	0x100e5b61c <__RNvMsn_NtCs6zY7rJBZy9D_5alloc4syncINtB5_3ArcNtNtNtCsgTc3P8JF8aG_8otter_vm6object18prototype_validity17PrototypeValidityE9drop_slowBM_>
100df0b4c:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0b50:     	ldr	w1, [x27]
100df0b54:     	mov	x0, x25
100df0b58:     	bl	0x100cf6f38 <__RNvMNtCsgTc3P8JF8aG_8otter_vm13call_feedbackNtB4_11Interpreter29record_ordinary_call_feedback>
100df0b5c:     	add	x0, sp, #0x3f8
100df0b60:     	bl	0x100c45ddc <__RINvNtCsjIpoRzUjyHQ_4core3ptr9drop_glueINtNtB4_6option6OptionNtCsgTc3P8JF8aG_8otter_vm10MethodSiteEEBZ_>
100df0b64:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0b68:     	str	xzr, [sp, #0x298]
100df0b6c:     	mov	w24, #0x0               ; =0
100df0b70:     	b	0x100df00bc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5afc>
100df0b74:     	str	xzr, [sp, #0x2a0]
100df0b78:     	mov	w24, #0x0               ; =0
100df0b7c:     	b	0x100df0158 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x5b98>
100df0b80:     	ldp	w3, w4, [x27, #0xc]
100df0b84:     	ldr	x2, [sp, #0x358]
100df0b88:     	sub	x0, x29, #0xd8
100df0b8c:     	mov	x1, x20
100df0b90:     	mov	x5, x24
100df0b94:     	bl	0x100cebc80 <__RNvMNtCsgTc3P8JF8aG_8otter_vm12iterator_opsNtB4_11Interpreter22run_iterator_next_regs>
100df0b98:     	ldurb	w8, [x29, #-0xd8]
100df0b9c:     	cmp	w8, #0xff
100df0ba0:     	b.ne	0x100df1e1c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x785c>
100df0ba4:     	mov	w8, #0x2                ; =2
100df0ba8:     	str	w8, [sp, #0x348]
100df0bac:     	b	0x100df0c44 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6684>
100df0bb0:     	mov	w5, #0x0                ; =0
100df0bb4:     	b	0x100df0be8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6628>
100df0bb8:     	mov	w5, #0x1                ; =1
100df0bbc:     	add	x0, sp, #0x420
100df0bc0:     	mov	x1, x20
100df0bc4:     	ldr	x2, [sp, #0x328]
100df0bc8:     	ldr	x3, [sp, #0x308]
100df0bcc:     	mov	x4, x28
100df0bd0:     	bl	0x100cd1ca4 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10module_opsNtB4_11Interpreter31ensure_deferred_namespace_ready>
100df0bd4:     	ldr	x8, [sp, #0x420]
100df0bd8:     	cmp	x8, #0x2
100df0bdc:     	b.eq	0x100deefc0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x4a00>
100df0be0:     	b	0x100df0eb0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68f0>
100df0be4:     	mov	w5, #0x1                ; =1
100df0be8:     	add	x0, sp, #0x420
100df0bec:     	mov	x1, x20
100df0bf0:     	ldr	x2, [sp, #0x328]
100df0bf4:     	ldr	x3, [sp, #0x308]
100df0bf8:     	mov	x4, x27
100df0bfc:     	bl	0x100cd1ca4 <__RNvMNtCsgTc3P8JF8aG_8otter_vm10module_opsNtB4_11Interpreter31ensure_deferred_namespace_ready>
100df0c00:     	ldr	x8, [sp, #0x420]
100df0c04:     	cmp	x8, #0x2
100df0c08:     	b.ne	0x100df0eb0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68f0>
100df0c0c:     	mov	x0, x19
100df0c10:     	ldr	x4, [sp, #0x360]
100df0c14:     	strb	w0, [sp]
100df0c18:     	add	x0, sp, #0x420
100df0c1c:     	mov	x1, x20
100df0c20:     	ldr	x2, [sp, #0x308]
100df0c24:     	ldr	x3, [sp, #0x328]
100df0c28:     	mov	x5, x24
100df0c2c:     	mov	x6, x23
100df0c30:     	mov	x7, x26
100df0c34:     	bl	0x100d0cb9c <__RNvMNtCsgTc3P8JF8aG_8otter_vm17property_dispatchNtB4_11Interpreter23run_delete_element_regs>
100df0c38:     	ldr	x8, [sp, #0x420]
100df0c3c:     	cmp	x8, #0x2
100df0c40:     	b.ne	0x100df0eb0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68f0>
100df0c44:     	ldr	x9, [sp, #0x328]
100df0c48:     	ldr	x8, [x9, #0x10]
100df0c4c:     	cmp	x8, #0x2
100df0c50:     	b.ne	0x100df0ebc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68fc>
100df0c54:     	ldrb	w8, [x9, #0x112]
100df0c58:     	cmp	w8, #0xff
100df0c5c:     	ldr	x0, [sp, #0x2f8]
100df0c60:     	ldr	x19, [sp, #0x308]
100df0c64:     	ldr	w22, [sp, #0x348]
100df0c68:     	b.eq	0x100dea888 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x2c8>
100df0c6c:     	b	0x100df0dd8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6818>
100df0c70:     	ldr	x23, [x19]
100df0c74:     	ldr	w8, [x19, #0x10]
100df0c78:     	subs	w8, w2, w8
100df0c7c:     	b.lo	0x100df0c90 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x66d0>
100df0c80:     	ldr	x9, [x23, #0x20]
100df0c84:     	ldr	x9, [x9, #0x50]
100df0c88:     	cmp	x9, x8
100df0c8c:     	b.hi	0x100df0d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x677c>
100df0c90:     	ldr	x8, [x23, #0x28]
100df0c94:     	ldp	w9, w10, [x8, #0x10]
100df0c98:     	cmp	w9, #0x0
100df0c9c:     	ccmp	w10, w2, #0x0, ne
100df0ca0:     	b.eq	0x100df0d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x677c>
100df0ca4:     	mov	x11, x19
100df0ca8:     	mov	x22, x6
100df0cac:     	mov	x19, x0
100df0cb0:     	cmp	w9, #0x0
100df0cb4:     	ccmp	w10, w2, #0x0, ne
100df0cb8:     	b.eq	0x100df0df4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6834>
100df0cbc:     	add	x0, sp, #0x420
100df0cc0:     	add	x1, x11, #0x8
100df0cc4:     	bl	0x100dbd3d8 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm17execution_contextNtB5_16ExecutionContext15for_function_in>
100df0cc8:     	add	x8, sp, #0x420
100df0ccc:     	ldr	q0, [x8]
100df0cd0:     	fmov	x23, d0
100df0cd4:     	cbz	x23, 0x100df0e08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6848>
100df0cd8:     	ldr	x8, [sp, #0x430]
100df0cdc:     	add	x19, sp, #0x420
100df0ce0:     	str	q0, [x19]
100df0ce4:     	str	x8, [sp, #0x430]
100df0ce8:     	mov	x0, x20
100df0cec:     	mov	x1, x23
100df0cf0:     	bl	0x100e50ba8 <__RNvMs_NtNtCsgTc3P8JF8aG_8otter_vm6interp8feedbackNtB8_11Interpreter30ensure_method_feedback_context>
100df0cf4:     	ldur	q0, [x19, #0x8]
100df0cf8:     	add	x8, sp, #0x279
100df0cfc:     	stur	q0, [x8, #0xff]
100df0d00:     	str	x23, [sp, #0x370]
100df0d04:     	add	x19, sp, #0x370
100df0d08:     	mov	w21, #0x1               ; =1
100df0d0c:     	ldr	w2, [sp, #0x36c]
100df0d10:     	ldr	w8, [x19, #0x10]
100df0d14:     	subs	w1, w2, w8
100df0d18:     	b.hs	0x100df0d50 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6790>
100df0d1c:     	b	0x100df0d68 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x67a8>
100df0d20:     	mov	x22, x6
100df0d24:     	mov	w21, #0x0               ; =0
100df0d28:     	ldr	x23, [x19]
100df0d2c:     	ldr	w8, [x19, #0x10]
100df0d30:     	subs	w1, w2, w8
100df0d34:     	b.hs	0x100df0d50 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6790>
100df0d38:     	b	0x100df0d68 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x67a8>
100df0d3c:     	mov	x22, x6
100df0d40:     	mov	w21, #0x0               ; =0
100df0d44:     	ldr	w8, [x19, #0x10]
100df0d48:     	subs	w1, w2, w8
100df0d4c:     	b.lo	0x100df0d68 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x67a8>
100df0d50:     	ldr	x8, [x23, #0x20]
100df0d54:     	ldr	x8, [x8, #0x50]
100df0d58:     	cmp	x8, x1
100df0d5c:     	b.ls	0x100df0d68 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x67a8>
100df0d60:     	ldr	x8, [x23, #0x28]
100df0d64:     	b	0x100df0d80 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x67c0>
100df0d68:     	ldr	x8, [x23, #0x28]
100df0d6c:     	ldp	w9, w10, [x8, #0x10]
100df0d70:     	cmp	w9, #0x0
100df0d74:     	ccmp	w10, w2, #0x0, ne
100df0d78:     	b.ne	0x100df0dbc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x67fc>
100df0d7c:     	mov	w1, #0x0                ; =0
100df0d80:     	add	x0, x8, #0x10
100df0d84:     	bl	0x100e1c500 <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm10executableNtB4_16ExecutableModule13function_slot>
100df0d88:     	cbz	x0, 0x100df0dbc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x67fc>
100df0d8c:     	str	w21, [sp, #0x2f4]
100df0d90:     	ldr	x8, [x0]
100df0d94:     	add	x25, x8, #0x10
100df0d98:     	ldr	x0, [sp, #0x2f8]
100df0d9c:     	ldr	x5, [sp, #0x328]
100df0da0:     	mov	x6, x22
100df0da4:     	ldr	x8, [x29, #0x10]
100df0da8:     	str	x8, [sp, #0x2e8]
100df0dac:     	ldr	w8, [x26, #0x1d8]
100df0db0:     	cmp	w8, w24
100df0db4:     	b.lo	0x100dea804 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x244>
100df0db8:     	b	0x100dea808 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x248>
100df0dbc:     	mov	w8, #0x1                ; =1
100df0dc0:     	ldr	x9, [sp, #0x2f8]
100df0dc4:     	strb	w8, [x9, #0x8]
100df0dc8:     	str	x8, [x9]
100df0dcc:     	tbz	w21, #0x0, 0x100df1dbc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x77fc>
100df0dd0:     	b	0x100df1d74 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x77b4>
100df0dd4:     	str	x9, [sp, #0x338]
100df0dd8:     	mov	w8, #0x18               ; =24
100df0ddc:     	strb	w8, [x0, #0x8]
100df0de0:     	str	xzr, [x0]
100df0de4:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df0de8:     	ldr	x9, [sp, #0x2f8]
100df0dec:     	strb	wzr, [x9, #0x8]
100df0df0:     	b	0x100df1d34 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7774>
100df0df4:     	ldr	x9, [x8, #0x60]
100df0df8:     	cbz	x9, 0x100df0e08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6848>
100df0dfc:     	ldr	x8, [x8, #0x58]
100df0e00:     	ldapr	x9, [x8]
100df0e04:     	cbz	x9, 0x100df0e98 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68d8>
100df0e08:     	str	xzr, [sp, #0x370]
100df0e0c:     	mov	w8, #0x1                ; =1
100df0e10:     	strb	w8, [x19, #0x8]
100df0e14:     	str	x8, [x19]
100df0e18:     	mov	x8, xzr
100df0e1c:     	b	0x100df1dbc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x77fc>
100df0e20:     	add	x8, sp, #0x420
100df0e24:     	ldr	q0, [x8]
100df0e28:     	ldr	x9, [sp, #0x2f8]
100df0e2c:     	stur	q0, [x9, #0x8]
100df0e30:     	ldr	x8, [sp, #0x430]
100df0e34:     	str	x8, [x9, #0x18]
100df0e38:     	b	0x100df1d34 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7774>
100df0e3c:     	str	xzr, [sp, #0x350]
100df0e40:     	add	x10, sp, #0x420
100df0e44:     	ldur	q0, [x10, #0x2]
100df0e48:     	ldr	x11, [sp, #0x2f8]
100df0e4c:     	stur	q0, [x11, #0xa]
100df0e50:     	ldr	x10, [sp, #0x430]
100df0e54:     	str	x10, [x11, #0x18]
100df0e58:     	strb	w9, [x11, #0x8]
100df0e5c:     	strb	w8, [x11, #0x9]
100df0e60:     	mov	w8, #0x1                ; =1
100df0e64:     	str	x8, [x11]
100df0e68:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df0e6c:     	ldr	w8, [x20, #0x118]
100df0e70:     	ldr	x9, [x20, #0x120]
100df0e74:     	str	xzr, [x20, #0x118]
100df0e78:     	cmp	w8, #0x0
100df0e7c:     	mov	w8, #0xa                ; =10
100df0e80:     	csel	x8, x9, x8, ne
100df0e84:     	mov	w9, #0x17               ; =23
100df0e88:     	strb	w9, [x0, #0x8]
100df0e8c:     	str	x8, [x0, #0x10]
100df0e90:     	str	xzr, [x0]
100df0e94:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df0e98:     	ldr	w2, [x8, #0x8]
100df0e9c:     	b	0x100df0cbc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x66fc>
100df0ea0:     	mov	x22, #0x0               ; =0
100df0ea4:     	mov	w26, #0x1               ; =1
100df0ea8:     	mov	w28, #0x1               ; =1
100df0eac:     	b	0x100df1d08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7748>
100df0eb0:     	cbnz	x8, 0x100df1348 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6d88>
100df0eb4:     	mov	w8, #0x1                ; =1
100df0eb8:     	b	0x100df134c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6d8c>
100df0ebc:     	ldr	x0, [sp, #0x2f8]
100df0ec0:     	b	0x100df0dd8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6818>
100df0ec4:     	cbnz	x8, 0x100df1264 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6ca4>
100df0ec8:     	mov	w8, #0x1                ; =1
100df0ecc:     	b	0x100df1268 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6ca8>
100df0ed0:     	mov	w28, #0x1               ; =1
100df0ed4:     	b	0x100df1d08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7748>
100df0ed8:     	ldrh	w8, [x27, #0x14]
100df0edc:     	ldr	x9, [sp, #0x358]
100df0ee0:     	ldr	w10, [x9, #0x18]
100df0ee4:     	cmp	w10, w8
100df0ee8:     	b.ls	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100df0eec:     	ldp	w25, w26, [x27, #0xc]
100df0ef0:     	ldr	x10, [x9, #0x10]
100df0ef4:     	ldr	x23, [x10, x8, lsl #3]
100df0ef8:     	ldr	x0, [x20, #0x7550]
100df0efc:     	ldr	x1, [x20, #0x7558]
100df0f00:     	ldr	w2, [x9, #0x64]
100df0f04:     	bl	0x100d97e70 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp6framesNtB6_11Interpreter21frame_generator_owner>
100df0f08:     	ldr	x19, [sp, #0x2f8]
100df0f0c:     	cmp	w0, #0x1
100df0f10:     	b.ne	0x100df1290 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6cd0>
100df0f14:     	mov	x24, x1
100df0f18:     	stur	w1, [x29, #-0x78]
100df0f1c:     	ldr	x0, [sp, #0x328]
100df0f20:     	bl	0x100df4820 <__RNvMs2_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStack8last_mut>
100df0f24:     	cbz	x0, 0x100df1408 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e48>
100df0f28:     	ldr	w8, [x0, #0x4]
100df0f2c:     	cmn	w8, #0x1
100df0f30:     	b.eq	0x100df1408 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e48>
100df0f34:     	add	w8, w8, #0x1
100df0f38:     	str	w8, [x0, #0x4]
100df0f3c:     	ldr	x8, [sp, #0x328]
100df0f40:     	ldr	x0, [x8, #0x180]
100df0f44:     	bl	0x100df4668 <__RNvMs2_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStack3pop>
100df0f48:     	mov	x28, x0
100df0f4c:     	cbz	x0, 0x100df1e60 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x78a0>
100df0f50:     	mov	x0, x20
100df0f54:     	mov	x1, x28
100df0f58:     	bl	0x100d977f4 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp6framesNtB6_11Interpreter17frame_detach_cold>
100df0f5c:     	mov	x27, x0
100df0f60:     	add	x0, sp, #0x420
100df0f64:     	mov	x1, x28
100df0f68:     	bl	0x100d97a14 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp6framesNtB6_11Interpreter17park_active_frame>
100df0f6c:     	sub	x0, x29, #0x78
100df0f70:     	add	x1, x20, #0x430
100df0f74:     	add	x2, sp, #0x420
100df0f78:     	mov	x3, x27
100df0f7c:     	mov	x4, x25
100df0f80:     	mov	x5, x26
100df0f84:     	mov	x6, x23
100df0f88:     	bl	0x100dc4bcc <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm9generatorNtB5_11JsGenerator16park_after_yield>
100df0f8c:     	adrp	x8, 0x1034f4000 <_global_added_lock+0x68>
100df0f90:     	add	x8, x8, #0xef8
100df0f94:     	ldr	x10, [x8]
100df0f98:     	mov	w9, w24
100df0f9c:     	add	x10, x10, x9
100df0fa0:     	ldrb	w10, [x10, #0x6e]
100df0fa4:     	tbz	w10, #0x0, 0x100df0fe4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6a24>
100df0fa8:     	ldr	x8, [x8]
100df0fac:     	add	x8, x8, x9
100df0fb0:     	mov	w9, #0x1                ; =1
100df0fb4:     	strb	w9, [x8, #0x6f]
100df0fb8:     	sub	x0, x29, #0xa8
100df0fbc:     	mov	x1, x20
100df0fc0:     	ldr	x2, [sp, #0x328]
100df0fc4:     	ldr	x3, [sp, #0x308]
100df0fc8:     	mov	x4, x24
100df0fcc:     	mov	x5, x23
100df0fd0:     	bl	0x100cf2fb0 <__RNvMNtCsgTc3P8JF8aG_8otter_vm12iterator_opsNtB4_11Interpreter29async_generator_yield_awaited>
100df0fd4:     	ldur	x8, [x29, #-0xa8]
100df0fd8:     	cbz	x8, 0x100df1400 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e40>
100df0fdc:     	cmp	x8, #0x2
100df0fe0:     	b.ne	0x100df1ad4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7514>
100df0fe4:     	mov	w8, #0x17               ; =23
100df0fe8:     	ldr	x9, [sp, #0x2f8]
100df0fec:     	strb	w8, [x9, #0x8]
100df0ff0:     	str	x23, [x9, #0x10]
100df0ff4:     	str	xzr, [x9]
100df0ff8:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df0ffc:     	ldrh	w8, [x27, #0x14]
100df1000:     	ldr	x9, [sp, #0x358]
100df1004:     	ldr	w10, [x9, #0x18]
100df1008:     	cmp	w10, w8
100df100c:     	b.ls	0x100df19fc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x743c>
100df1010:     	ldp	w24, w25, [x27, #0xc]
100df1014:     	ldr	x10, [x9, #0x10]
100df1018:     	ldr	x23, [x10, x8, lsl #3]
100df101c:     	ldr	x0, [x20, #0x7550]
100df1020:     	ldr	x1, [x20, #0x7558]
100df1024:     	ldr	w2, [x9, #0x64]
100df1028:     	bl	0x100d97e70 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp6framesNtB6_11Interpreter21frame_generator_owner>
100df102c:     	ldr	x19, [sp, #0x2f8]
100df1030:     	cmp	w0, #0x1
100df1034:     	b.ne	0x100df1290 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6cd0>
100df1038:     	mov	x26, x1
100df103c:     	stur	w1, [x29, #-0x78]
100df1040:     	ldr	x0, [sp, #0x328]
100df1044:     	bl	0x100df4820 <__RNvMs2_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStack8last_mut>
100df1048:     	cbz	x0, 0x100df1408 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e48>
100df104c:     	ldr	w8, [x0, #0x4]
100df1050:     	cmn	w8, #0x1
100df1054:     	b.eq	0x100df1408 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e48>
100df1058:     	add	w8, w8, #0x1
100df105c:     	str	w8, [x0, #0x4]
100df1060:     	ldr	x8, [sp, #0x328]
100df1064:     	ldr	x0, [x8, #0x180]
100df1068:     	bl	0x100df4668 <__RNvMs2_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStack3pop>
100df106c:     	mov	x28, x0
100df1070:     	cbz	x0, 0x100df1e6c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x78ac>
100df1074:     	mov	x0, x20
100df1078:     	mov	x1, x28
100df107c:     	bl	0x100d977f4 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp6framesNtB6_11Interpreter17frame_detach_cold>
100df1080:     	mov	x27, x0
100df1084:     	add	x0, sp, #0x420
100df1088:     	mov	x1, x28
100df108c:     	bl	0x100d97a14 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp6framesNtB6_11Interpreter17park_active_frame>
100df1090:     	sub	x0, x29, #0x78
100df1094:     	add	x1, x20, #0x430
100df1098:     	add	x2, sp, #0x420
100df109c:     	mov	x3, x27
100df10a0:     	mov	x4, x24
100df10a4:     	mov	x5, x25
100df10a8:     	mov	x6, x23
100df10ac:     	bl	0x100dc6258 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm9generatorNtB5_11JsGenerator25park_after_yield_delegate>
100df10b0:     	adrp	x8, 0x1034f4000 <_global_added_lock+0x68>
100df10b4:     	add	x8, x8, #0xef8
100df10b8:     	ldr	x10, [x8]
100df10bc:     	mov	w9, w26
100df10c0:     	add	x10, x10, x9
100df10c4:     	ldrb	w10, [x10, #0x6e]
100df10c8:     	cbz	w10, 0x100df1ab8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x74f8>
100df10cc:     	ldr	x8, [x8]
100df10d0:     	add	x8, x8, x9
100df10d4:     	mov	w9, #0x1                ; =1
100df10d8:     	strb	w9, [x8, #0x6f]
100df10dc:     	sub	x0, x29, #0xa8
100df10e0:     	sub	x3, x29, #0x78
100df10e4:     	mov	x1, x20
100df10e8:     	ldr	x2, [sp, #0x308]
100df10ec:     	mov	x4, #0x0                ; =0
100df10f0:     	mov	x5, x23
100df10f4:     	mov	w6, #0x0                ; =0
100df10f8:     	bl	0x100cf2998 <__RNvMNtCsgTc3P8JF8aG_8otter_vm12iterator_opsNtB4_11Interpreter29async_generator_complete_step>
100df10fc:     	ldurb	w8, [x29, #-0xa8]
100df1100:     	cmp	w8, #0xff
100df1104:     	b.eq	0x100df1a90 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x74d0>
100df1108:     	ldur	q0, [x29, #-0xa8]
100df110c:     	ldr	x9, [sp, #0x2f8]
100df1110:     	stur	q0, [x9, #0x8]
100df1114:     	ldur	x8, [x29, #-0x98]
100df1118:     	b	0x100df0e34 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6874>
100df111c:     	ldr	w8, [x27, #0xc]
100df1120:     	mov	w9, #0xd                ; =13
100df1124:     	ldr	x10, [sp, #0x2f8]
100df1128:     	strb	w9, [x10, #0x8]
100df112c:     	str	w8, [x10, #0xc]
100df1130:     	mov	w8, #0x1                ; =1
100df1134:     	str	x8, [x10]
100df1138:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df113c:     	ldr	x8, [sp, #0x358]
100df1140:     	ldr	x0, [x20, #0x7550]
100df1144:     	ldr	x1, [x20, #0x7558]
100df1148:     	ldr	w2, [x8, #0x64]
100df114c:     	bl	0x100d97e70 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp6framesNtB6_11Interpreter21frame_generator_owner>
100df1150:     	ldr	x21, [sp, #0x2f8]
100df1154:     	cmp	w0, #0x1
100df1158:     	b.ne	0x100df12a4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6ce4>
100df115c:     	mov	x23, x1
100df1160:     	stur	w1, [x29, #-0x78]
100df1164:     	ldr	x8, [sp, #0x358]
100df1168:     	ldr	x24, [x8, #0x30]
100df116c:     	ldr	x0, [sp, #0x328]
100df1170:     	bl	0x100df4820 <__RNvMs2_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStack8last_mut>
100df1174:     	cbz	x0, 0x100df1418 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e58>
100df1178:     	ldr	w8, [x0, #0x4]
100df117c:     	cmn	w8, #0x1
100df1180:     	b.eq	0x100df1418 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e58>
100df1184:     	add	w8, w8, #0x1
100df1188:     	str	w8, [x0, #0x4]
100df118c:     	ldr	x8, [sp, #0x328]
100df1190:     	ldr	x0, [x8, #0x180]
100df1194:     	bl	0x100df4668 <__RNvMs2_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStack3pop>
100df1198:     	mov	x26, x0
100df119c:     	cbz	x0, 0x100df1e54 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7894>
100df11a0:     	mov	x0, x20
100df11a4:     	mov	x1, x26
100df11a8:     	bl	0x100d977f4 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp6framesNtB6_11Interpreter17frame_detach_cold>
100df11ac:     	mov	x25, x0
100df11b0:     	add	x0, sp, #0x420
100df11b4:     	mov	x1, x26
100df11b8:     	bl	0x100d97a14 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp6framesNtB6_11Interpreter17park_active_frame>
100df11bc:     	sub	x0, x29, #0x78
100df11c0:     	add	x1, x20, #0x430
100df11c4:     	add	x2, sp, #0x420
100df11c8:     	mov	x3, x25
100df11cc:     	bl	0x100dc3d44 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm9generatorNtB5_11JsGenerator10park_frame>
100df11d0:     	ldr	w6, [sp, #0x36c]
100df11d4:     	sub	x19, x29, #0xa8
100df11d8:     	sub	x0, x29, #0xa8
100df11dc:     	mov	x1, x20
100df11e0:     	ldr	x2, [sp, #0x328]
100df11e4:     	ldr	x3, [sp, #0x308]
100df11e8:     	mov	x4, x23
100df11ec:     	mov	x5, x24
100df11f0:     	bl	0x100d982a0 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp6framesNtB6_11Interpreter25resolve_started_generator>
100df11f4:     	ldur	x8, [x29, #-0xa8]
100df11f8:     	cbz	x8, 0x100df1518 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6f58>
100df11fc:     	cmp	x8, #0x2
100df1200:     	b.ne	0x100df1520 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6f60>
100df1204:     	ldur	x8, [x29, #-0xa0]
100df1208:     	mov	w9, #0x17               ; =23
100df120c:     	strb	w9, [x21, #0x8]
100df1210:     	str	x8, [x21, #0x10]
100df1214:     	str	xzr, [x21]
100df1218:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df121c:     	mov	w8, #0x1                ; =1
100df1220:     	ldr	x11, [sp, #0x2f8]
100df1224:     	b	0x100df137c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6dbc>
100df1228:     	mov	w8, #0x1                ; =1
100df122c:     	ldr	x9, [sp, #0x2f8]
100df1230:     	strb	w8, [x9, #0x8]
100df1234:     	ldr	w11, [sp, #0x348]
100df1238:     	ldr	x10, [sp, #0x2d0]
100df123c:     	stp	w11, w10, [x9, #0xc]
100df1240:     	b	0x100df1d38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7778>
100df1244:     	mov	w8, #0x1                ; =1
100df1248:     	b	0x100df1338 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6d78>
100df124c:     	mov	w8, #0x1                ; =1
100df1250:     	ldr	x9, [sp, #0x2f8]
100df1254:     	strb	w8, [x9, #0x8]
100df1258:     	ldr	x10, [sp, #0x250]
100df125c:     	str	x10, [x9, #0x10]
100df1260:     	b	0x100df1d38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7778>
100df1264:     	mov	x8, #0x0                ; =0
100df1268:     	add	x9, sp, #0x420
100df126c:     	ldur	q0, [x9, #0x8]
100df1270:     	ldr	x10, [sp, #0x2f8]
100df1274:     	stur	q0, [x10, #0x8]
100df1278:     	ldur	x9, [x9, #0x18]
100df127c:     	b	0x100df1360 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6da0>
100df1280:     	mov	x22, #0x0               ; =0
100df1284:     	mov	x26, #0x0               ; =0
100df1288:     	mov	w28, #0x2               ; =2
100df128c:     	b	0x100df1d00 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7740>
100df1290:     	mov	w8, #0x2                ; =2
100df1294:     	strb	w8, [x19, #0x8]
100df1298:     	mov	w8, #0x1                ; =1
100df129c:     	str	x8, [x19]
100df12a0:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df12a4:     	mov	w8, #0x2                ; =2
100df12a8:     	strb	w8, [x21, #0x8]
100df12ac:     	mov	w8, #0x1                ; =1
100df12b0:     	str	x8, [x21]
100df12b4:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df12b8:     	mov	w8, #0x1                ; =1
100df12bc:     	b	0x100df1440 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e80>
100df12c0:     	add	x8, sp, #0x420
100df12c4:     	ldur	w8, [x8, #0x1]
100df12c8:     	ldr	x14, [sp, #0x2f8]
100df12cc:     	stur	w8, [x14, #0x9]
100df12d0:     	ldr	w8, [sp, #0x424]
100df12d4:     	str	w8, [x14, #0xc]
100df12d8:     	ldr	x10, [sp, #0x428]
100df12dc:     	ldr	x8, [sp, #0x430]
100df12e0:     	strb	w9, [x14, #0x8]
100df12e4:     	str	x10, [x14, #0x10]
100df12e8:     	mov	w9, #0x1                ; =1
100df12ec:     	mov	w10, #0x18              ; =24
100df12f0:     	b	0x100df130c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6d4c>
100df12f4:     	mov	x9, #0x0                ; =0
100df12f8:     	ldr	x8, [sp, #0x430]
100df12fc:     	mov	w10, #0x19              ; =25
100df1300:     	ldr	x14, [sp, #0x2f8]
100df1304:     	strb	w10, [x14, #0x8]
100df1308:     	mov	w10, #0x10              ; =16
100df130c:     	ldr	w11, [sp, #0x2f4]
100df1310:     	ldr	x12, [sp, #0x2e8]
100df1314:     	ldr	x13, [sp, #0x350]
100df1318:     	str	x8, [x14, x10]
100df131c:     	str	x9, [x14]
100df1320:     	ldr	x8, [x12]
100df1324:     	add	x8, x8, x13
100df1328:     	str	x8, [x12]
100df132c:     	tbnz	w11, #0x0, 0x100df1d74 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x77b4>
100df1330:     	b	0x100df1dbc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x77fc>
100df1334:     	mov	w8, #0xa                ; =10
100df1338:     	ldr	x9, [sp, #0x2f8]
100df133c:     	strb	w8, [x9, #0x8]
100df1340:     	b	0x100df1d34 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7774>
100df1344:     	cbz	x8, 0x100df0eb4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x68f4>
100df1348:     	mov	x8, #0x0                ; =0
100df134c:     	add	x9, sp, #0x420
100df1350:     	ldur	q0, [x9, #0x8]
100df1354:     	ldr	x10, [sp, #0x2f8]
100df1358:     	stur	q0, [x10, #0x8]
100df135c:     	ldr	x9, [sp, #0x438]
100df1360:     	str	x9, [x10, #0x18]
100df1364:     	str	x8, [x10]
100df1368:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df136c:     	cbnz	x8, 0x100df1428 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e68>
100df1370:     	mov	w8, #0x1                ; =1
100df1374:     	b	0x100df142c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e6c>
100df1378:     	mov	w8, #0xa                ; =10
100df137c:     	strb	w8, [x11, #0x8]
100df1380:     	mov	w8, #0x1                ; =1
100df1384:     	str	x8, [x11]
100df1388:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df138c:     	ldr	x9, [sp, #0x430]
100df1390:     	ldr	x11, [sp, #0x438]
100df1394:     	lsr	x8, x9, #8
100df1398:     	mov	x28, x24
100df139c:     	mov	x22, x24
100df13a0:     	mov	x10, x9
100df13a4:     	b	0x100df1d00 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7740>
100df13a8:     	mov	w8, #0x1                ; =1
100df13ac:     	ldr	x9, [sp, #0x2f8]
100df13b0:     	str	xzr, [x9, #0x18]
100df13b4:     	stp	x8, x8, [x9]
100df13b8:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df13bc:     	ldrb	w8, [sp, #0x437]
100df13c0:     	add	x10, sp, #0x420
100df13c4:     	ldurh	w9, [x10, #0x15]
100df13c8:     	orr	w8, w9, w8, lsl #16
100df13cc:     	ldur	w9, [x10, #0x11]
100df13d0:     	orr	x8, x9, x8, lsl #32
100df13d4:     	ldr	x11, [sp, #0x438]
100df13d8:     	mov	x28, x19
100df13dc:     	mov	x22, x19
100df13e0:     	str	x11, [sp, #0x258]
100df13e4:     	b	0x100df1d08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7748>
100df13e8:     	mov	w8, #0x1                ; =1
100df13ec:     	ldr	x9, [sp, #0x2f8]
100df13f0:     	strb	w8, [x9, #0x8]
100df13f4:     	str	x13, [x9, #0x10]
100df13f8:     	b	0x100df1d38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7778>
100df13fc:     	cbnz	x8, 0x100df1ad4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7514>
100df1400:     	mov	w8, #0x1                ; =1
100df1404:     	b	0x100df1ad8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7518>
100df1408:     	mov	w8, #0x1                ; =1
100df140c:     	strb	w8, [x19, #0x8]
100df1410:     	str	x8, [x19]
100df1414:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df1418:     	mov	w8, #0x1                ; =1
100df141c:     	strb	w8, [x21, #0x8]
100df1420:     	str	x8, [x21]
100df1424:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df1428:     	mov	x8, #0x0                ; =0
100df142c:     	ldr	x10, [sp, #0x2f8]
100df1430:     	add	x9, sp, #0x420
100df1434:     	ldur	q0, [x9, #0x8]
100df1438:     	b	0x100df1358 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6d98>
100df143c:     	mov	w8, #0xa                ; =10
100df1440:     	strb	w8, [x12, #0x8]
100df1444:     	mov	w8, #0x1                ; =1
100df1448:     	str	x8, [x12]
100df144c:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df1450:     	mov	w8, #0x1                ; =1
100df1454:     	ldr	x9, [sp, #0x2f8]
100df1458:     	strb	w8, [x9, #0x8]
100df145c:     	ldr	x10, [sp, #0x240]
100df1460:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1464:     	add	x11, sp, #0x420
100df1468:     	ldur	w9, [x11, #0x1]
100df146c:     	ldr	x10, [sp, #0x2f8]
100df1470:     	stur	w9, [x10, #0x9]
100df1474:     	ldr	w9, [sp, #0x424]
100df1478:     	str	w9, [x10, #0xc]
100df147c:     	ldur	q0, [x11, #0x8]
100df1480:     	strb	w8, [x10, #0x8]
100df1484:     	str	q0, [x10, #0x10]
100df1488:     	mov	w8, #0x1                ; =1
100df148c:     	str	x8, [x10]
100df1490:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df1494:     	add	x11, sp, #0x420
100df1498:     	ldur	w9, [x11, #0x1]
100df149c:     	ldr	x10, [sp, #0x2f8]
100df14a0:     	stur	w9, [x10, #0x9]
100df14a4:     	ldr	w9, [sp, #0x424]
100df14a8:     	str	w9, [x10, #0xc]
100df14ac:     	ldur	q0, [x11, #0x8]
100df14b0:     	strb	w8, [x10, #0x8]
100df14b4:     	str	q0, [x10, #0x10]
100df14b8:     	b	0x100df1130 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6b70>
100df14bc:     	add	x8, sp, #0x420
100df14c0:     	ldur	w8, [x8, #0x1]
100df14c4:     	ldr	x11, [sp, #0x2f8]
100df14c8:     	stur	w8, [x11, #0x9]
100df14cc:     	ldr	w8, [sp, #0x424]
100df14d0:     	str	w8, [x11, #0xc]
100df14d4:     	ldr	x10, [sp, #0x428]
100df14d8:     	ldr	x8, [sp, #0x430]
100df14dc:     	strb	w9, [x11, #0x8]
100df14e0:     	str	x10, [x11, #0x10]
100df14e4:     	mov	w9, #0x1                ; =1
100df14e8:     	mov	w10, #0x18              ; =24
100df14ec:     	b	0x100df1508 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6f48>
100df14f0:     	mov	x9, #0x0                ; =0
100df14f4:     	ldr	x8, [sp, #0x430]
100df14f8:     	mov	w10, #0x17              ; =23
100df14fc:     	ldr	x11, [sp, #0x2f8]
100df1500:     	strb	w10, [x11, #0x8]
100df1504:     	mov	w10, #0x10              ; =16
100df1508:     	ldr	x11, [sp, #0x2f8]
100df150c:     	str	x8, [x11, x10]
100df1510:     	str	x9, [x11]
100df1514:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df1518:     	mov	w8, #0x1                ; =1
100df151c:     	b	0x100df1524 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6f64>
100df1520:     	mov	x8, #0x0                ; =0
100df1524:     	ldur	q0, [x19, #0x8]
100df1528:     	stur	q0, [x21, #0x8]
100df152c:     	ldur	x9, [x19, #0x18]
100df1530:     	str	x9, [x21, #0x18]
100df1534:     	str	x8, [x21]
100df1538:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df153c:     	mov	w8, #0x1                ; =1
100df1540:     	ldr	x9, [sp, #0x2f8]
100df1544:     	strb	w8, [x9, #0x8]
100df1548:     	ldr	x10, [sp, #0x210]
100df154c:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1550:     	mov	w8, #0x1                ; =1
100df1554:     	ldr	x9, [sp, #0x2f8]
100df1558:     	strb	w8, [x9, #0x8]
100df155c:     	ldr	x10, [sp, #0x208]
100df1560:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1564:     	ldur	q0, [x29, #-0xa8]
100df1568:     	ldr	x9, [sp, #0x2f8]
100df156c:     	stur	q0, [x9, #0x8]
100df1570:     	ldur	x8, [x29, #-0x98]
100df1574:     	str	x8, [x9, #0x18]
100df1578:     	mov	w8, #0x1                ; =1
100df157c:     	ldr	x9, [sp, #0x2f8]
100df1580:     	b	0x100df1d38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7778>
100df1584:     	mov	w8, #0x1                ; =1
100df1588:     	ldr	x10, [sp, #0x2f8]
100df158c:     	strb	w8, [x10, #0x8]
100df1590:     	ldr	x11, [sp, #0xb0]
100df1594:     	stur	w11, [x10, #0x9]
100df1598:     	lsr	x9, x11, #48
100df159c:     	strb	w9, [x10, #0xf]
100df15a0:     	lsr	x9, x11, #32
100df15a4:     	sturh	w9, [x10, #0xd]
100df15a8:     	ldr	x9, [sp, #0x178]
100df15ac:     	str	x9, [x10, #0x10]
100df15b0:     	ldr	w9, [sp, #0x184]
100df15b4:     	str	w9, [x10, #0x18]
100df15b8:     	str	x8, [x10]
100df15bc:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df15c0:     	mov	w8, #0x1                ; =1
100df15c4:     	ldr	x9, [sp, #0x2f8]
100df15c8:     	strb	w8, [x9, #0x8]
100df15cc:     	ldr	x10, [sp, #0x1e8]
100df15d0:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df15d4:     	mov	w8, #0x1                ; =1
100df15d8:     	ldr	x9, [sp, #0x2f8]
100df15dc:     	strb	w8, [x9, #0x8]
100df15e0:     	ldr	x10, [sp, #0x200]
100df15e4:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df15e8:     	mov	w8, #0x1                ; =1
100df15ec:     	ldr	x9, [sp, #0x2f8]
100df15f0:     	strb	w8, [x9, #0x8]
100df15f4:     	ldr	x10, [sp, #0x1f8]
100df15f8:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df15fc:     	mov	w8, #0x1                ; =1
100df1600:     	ldr	x9, [sp, #0x2f8]
100df1604:     	strb	w8, [x9, #0x8]
100df1608:     	ldr	x10, [sp, #0x1f0]
100df160c:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1610:     	cbnz	x8, 0x100df1938 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7378>
100df1614:     	mov	w8, #0x1                ; =1
100df1618:     	b	0x100df193c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x737c>
100df161c:     	cbnz	x8, 0x100df1a20 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7460>
100df1620:     	add	x8, sp, #0x420
100df1624:     	ldur	q0, [x8, #0x8]
100df1628:     	ldr	x9, [sp, #0x2f8]
100df162c:     	stur	q0, [x9, #0x8]
100df1630:     	ldr	x8, [sp, #0x438]
100df1634:     	b	0x100df0e34 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6874>
100df1638:     	mov	w0, #0x29               ; =41
100df163c:     	mov	w1, #0x1                ; =1
100df1640:     	bl	0x101baac00 <_mi_malloc_aligned>
100df1644:     	cbz	x0, 0x100df1eb0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x78f0>
100df1648:     	adrp	x8, 0x102d10000 <_anon.7a775211447c4b11c21165a1336b249e.803+0x4ae>
100df164c:     	add	x8, x8, #0x6aa
100df1650:     	ldp	q0, q1, [x8]
100df1654:     	stp	q0, q1, [x0]
100df1658:     	ldur	q0, [x8, #0x19]
100df165c:     	stur	q0, [x0, #0x19]
100df1660:     	mov	w8, #0x29               ; =41
100df1664:     	str	x8, [sp, #0x420]
100df1668:     	str	x0, [sp, #0x428]
100df166c:     	str	x8, [sp, #0x430]
100df1670:     	add	x0, sp, #0x420
100df1674:     	bl	0x100024530 <__RNvXsO_NtCs6zY7rJBZy9D_5alloc6stringINtNtB7_5boxed3BoxeEINtNtCsjIpoRzUjyHQ_4core7convert4FromNtB5_6StringE4from>
100df1678:     	ldr	x19, [sp, #0x2f8]
100df167c:     	mov	x2, x0
100df1680:     	mov	x3, x1
100df1684:     	add	x8, sp, #0x420
100df1688:     	mov	x0, x20
100df168c:     	mov	x1, x2
100df1690:     	mov	x2, x3
100df1694:     	bl	0x100d97674 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp6errorsNtB6_11Interpreter8err_type>
100df1698:     	add	x8, sp, #0x420
100df169c:     	ldr	q0, [x8]
100df16a0:     	stur	q0, [x19, #0x8]
100df16a4:     	ldr	x8, [sp, #0x430]
100df16a8:     	str	x8, [x19, #0x18]
100df16ac:     	mov	w8, #0x1                ; =1
100df16b0:     	str	x8, [x19]
100df16b4:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df16b8:     	mov	w8, #0x1                ; =1
100df16bc:     	ldr	x9, [sp, #0x2f8]
100df16c0:     	strb	w8, [x9, #0x8]
100df16c4:     	str	x23, [x9, #0x10]
100df16c8:     	b	0x100df1d38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7778>
100df16cc:     	mov	w8, #0x1                ; =1
100df16d0:     	ldr	x9, [sp, #0x2f8]
100df16d4:     	strb	w8, [x9, #0x8]
100df16d8:     	ldr	x10, [sp, #0x1a0]
100df16dc:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df16e0:     	mov	w8, #0x1                ; =1
100df16e4:     	ldr	x9, [sp, #0x2f8]
100df16e8:     	strb	w8, [x9, #0x8]
100df16ec:     	ldr	x10, [sp, #0x198]
100df16f0:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df16f4:     	mov	w8, #0x1                ; =1
100df16f8:     	ldr	x9, [sp, #0x2f8]
100df16fc:     	strb	w8, [x9, #0x8]
100df1700:     	ldr	x10, [sp, #0x150]
100df1704:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1708:     	mov	w8, #0x1                ; =1
100df170c:     	ldr	x9, [sp, #0x2f8]
100df1710:     	strb	w8, [x9, #0x8]
100df1714:     	ldr	x10, [sp, #0xa8]
100df1718:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df171c:     	mov	w8, #0x1                ; =1
100df1720:     	ldr	x9, [sp, #0x2f8]
100df1724:     	strb	w8, [x9, #0x8]
100df1728:     	ldr	x10, [sp, #0x108]
100df172c:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1730:     	mov	w8, #0x1                ; =1
100df1734:     	ldr	x9, [sp, #0x2f8]
100df1738:     	strb	w8, [x9, #0x8]
100df173c:     	ldr	x10, [sp, #0x188]
100df1740:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1744:     	mov	w8, #0x1                ; =1
100df1748:     	ldr	x9, [sp, #0x2f8]
100df174c:     	strb	w8, [x9, #0x8]
100df1750:     	ldr	x10, [sp, #0x120]
100df1754:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1758:     	mov	w8, #0x1                ; =1
100df175c:     	ldr	x9, [sp, #0x2f8]
100df1760:     	strb	w8, [x9, #0x8]
100df1764:     	ldr	x10, [sp, #0x98]
100df1768:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df176c:     	mov	w8, #0x1                ; =1
100df1770:     	ldr	x9, [sp, #0x2f8]
100df1774:     	strb	w8, [x9, #0x8]
100df1778:     	ldr	x10, [sp, #0x138]
100df177c:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1780:     	mov	w8, #0x1                ; =1
100df1784:     	ldr	x9, [sp, #0x2f8]
100df1788:     	strb	w8, [x9, #0x8]
100df178c:     	ldr	x10, [sp, #0x110]
100df1790:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1794:     	mov	w8, #0x1                ; =1
100df1798:     	ldr	x9, [sp, #0x2f8]
100df179c:     	strb	w8, [x9, #0x8]
100df17a0:     	ldr	x10, [sp, #0x160]
100df17a4:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df17a8:     	mov	w8, #0x1                ; =1
100df17ac:     	ldr	x9, [sp, #0x2f8]
100df17b0:     	strb	w8, [x9, #0x8]
100df17b4:     	ldr	x10, [sp, #0x100]
100df17b8:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df17bc:     	ldr	x8, [sp, #0x2f8]
100df17c0:     	strb	w19, [x8, #0x8]
100df17c4:     	ldr	x9, [sp, #0x288]
100df17c8:     	b	0x100df1814 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7254>
100df17cc:     	mov	w8, #0x1                ; =1
100df17d0:     	ldr	x9, [sp, #0x2f8]
100df17d4:     	strb	w8, [x9, #0x8]
100df17d8:     	ldr	x10, [sp, #0x130]
100df17dc:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df17e0:     	mov	w8, #0x1                ; =1
100df17e4:     	ldr	x9, [sp, #0x2f8]
100df17e8:     	strb	w8, [x9, #0x8]
100df17ec:     	ldr	x10, [sp, #0x140]
100df17f0:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df17f4:     	mov	w8, #0x1                ; =1
100df17f8:     	ldr	x9, [sp, #0x2f8]
100df17fc:     	strb	w8, [x9, #0x8]
100df1800:     	ldr	x10, [sp, #0xb8]
100df1804:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1808:     	ldr	x8, [sp, #0x2f8]
100df180c:     	strb	w19, [x8, #0x8]
100df1810:     	ldr	x9, [sp, #0x280]
100df1814:     	str	w9, [x8, #0xc]
100df1818:     	str	x19, [x8]
100df181c:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df1820:     	mov	w8, #0x1                ; =1
100df1824:     	ldr	x9, [sp, #0x2f8]
100df1828:     	strb	w8, [x9, #0x8]
100df182c:     	ldr	x10, [sp, #0x118]
100df1830:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1834:     	mov	w8, #0x1                ; =1
100df1838:     	ldr	x9, [sp, #0x2f8]
100df183c:     	strb	w8, [x9, #0x8]
100df1840:     	ldr	x10, [sp, #0x218]
100df1844:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1848:     	mov	w8, #0x1                ; =1
100df184c:     	ldr	x9, [sp, #0x2f8]
100df1850:     	strb	w8, [x9, #0x8]
100df1854:     	ldr	x10, [sp, #0x158]
100df1858:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df185c:     	mov	w8, #0x1                ; =1
100df1860:     	ldr	x9, [sp, #0x2f8]
100df1864:     	strb	w8, [x9, #0x8]
100df1868:     	ldr	x10, [sp, #0x168]
100df186c:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1870:     	mov	w8, #0x1                ; =1
100df1874:     	ldr	x9, [sp, #0x2f8]
100df1878:     	strb	w8, [x9, #0x8]
100df187c:     	ldr	x10, [sp, #0x170]
100df1880:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1884:     	mov	w8, #0x1                ; =1
100df1888:     	ldr	x9, [sp, #0x2f8]
100df188c:     	strb	w8, [x9, #0x8]
100df1890:     	ldr	x10, [sp, #0x128]
100df1894:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1898:     	mov	w8, #0x1                ; =1
100df189c:     	ldr	x9, [sp, #0x2f8]
100df18a0:     	strb	w8, [x9, #0x8]
100df18a4:     	ldr	x10, [sp, #0xf8]
100df18a8:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df18ac:     	mov	w8, #0x1                ; =1
100df18b0:     	ldr	x9, [sp, #0x2f8]
100df18b4:     	strb	w8, [x9, #0x8]
100df18b8:     	ldr	x10, [sp, #0x190]
100df18bc:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df18c0:     	mov	w8, #0x1                ; =1
100df18c4:     	ldr	x9, [sp, #0x2f8]
100df18c8:     	strb	w8, [x9, #0x8]
100df18cc:     	ldr	x10, [sp, #0x148]
100df18d0:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df18d4:     	mov	w8, #0x1                ; =1
100df18d8:     	ldr	x9, [sp, #0x2f8]
100df18dc:     	strb	w8, [x9, #0x8]
100df18e0:     	ldr	x10, [sp, #0xf0]
100df18e4:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df18e8:     	mov	w8, #0x1                ; =1
100df18ec:     	ldr	x9, [sp, #0x2f8]
100df18f0:     	strb	w8, [x9, #0x8]
100df18f4:     	ldr	x10, [sp, #0xa0]
100df18f8:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df18fc:     	add	x9, sp, #0x420
100df1900:     	ldur	q0, [x9, #0x8]
100df1904:     	ldr	x10, [sp, #0x2f8]
100df1908:     	stur	q0, [x10, #0x8]
100df190c:     	ldr	x9, [sp, #0x438]
100df1910:     	str	x9, [x10, #0x18]
100df1914:     	mvn	w8, w8
100df1918:     	and	x8, x8, #0x1
100df191c:     	str	x8, [x10]
100df1920:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df1924:     	mov	w8, #0x1                ; =1
100df1928:     	ldr	x9, [sp, #0x2f8]
100df192c:     	strb	w8, [x9, #0x8]
100df1930:     	ldr	x10, [sp, #0xe8]
100df1934:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1938:     	mov	x8, #0x0                ; =0
100df193c:     	add	x9, sp, #0x420
100df1940:     	ldur	q0, [x9, #0x8]
100df1944:     	ldr	x10, [sp, #0x2f8]
100df1948:     	stur	q0, [x10, #0x8]
100df194c:     	ldr	x9, [sp, #0x438]
100df1950:     	str	x9, [x10, #0x18]
100df1954:     	str	x8, [x10]
100df1958:     	add	x0, sp, #0x3f8
100df195c:     	bl	0x100c45ddc <__RINvNtCsjIpoRzUjyHQ_4core3ptr9drop_glueINtNtB4_6option6OptionNtCsgTc3P8JF8aG_8otter_vm10MethodSiteEEBZ_>
100df1960:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df1964:     	mov	w8, #0x1                ; =1
100df1968:     	ldr	x9, [sp, #0x2f8]
100df196c:     	strb	w8, [x9, #0x8]
100df1970:     	ldr	x10, [sp, #0xd8]
100df1974:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1978:     	mov	w8, #0x1                ; =1
100df197c:     	ldr	x9, [sp, #0x2f8]
100df1980:     	strb	w8, [x9, #0x8]
100df1984:     	ldr	x10, [sp, #0xe0]
100df1988:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df198c:     	mov	w8, #0x1                ; =1
100df1990:     	ldr	x9, [sp, #0x2f8]
100df1994:     	strb	w8, [x9, #0x8]
100df1998:     	ldr	x10, [sp, #0x90]
100df199c:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df19a0:     	mov	w8, #0x1                ; =1
100df19a4:     	ldr	x9, [sp, #0x2f8]
100df19a8:     	strb	w8, [x9, #0x8]
100df19ac:     	ldr	x10, [sp, #0x88]
100df19b0:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df19b4:     	mov	w8, #0x1                ; =1
100df19b8:     	stp	x8, x23, [x20, #0x108]
100df19bc:     	ldur	q0, [x29, #-0xa8]
100df19c0:     	ldr	x10, [sp, #0x2f8]
100df19c4:     	stur	q0, [x10, #0x8]
100df19c8:     	ldur	x9, [x29, #-0x98]
100df19cc:     	b	0x100df1360 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6da0>
100df19d0:     	mov	w8, #0x1                ; =1
100df19d4:     	ldr	x9, [sp, #0x2f8]
100df19d8:     	strb	w8, [x9, #0x8]
100df19dc:     	ldr	x10, [sp, #0xc8]
100df19e0:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df19e4:     	mov	w8, #0x1                ; =1
100df19e8:     	ldr	x9, [sp, #0x2f8]
100df19ec:     	strb	w8, [x9, #0x8]
100df19f0:     	ldr	x10, [sp, #0xd0]
100df19f4:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df19f8:     	cmp	w8, #0x3
100df19fc:     	mov	w8, #0x1                ; =1
100df1a00:     	ldr	x9, [sp, #0x2f8]
100df1a04:     	strb	w8, [x9, #0x8]
100df1a08:     	b	0x100df1d38 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7778>
100df1a0c:     	mov	w8, #0x1                ; =1
100df1a10:     	ldr	x9, [sp, #0x2f8]
100df1a14:     	strb	w8, [x9, #0x8]
100df1a18:     	ldr	x10, [sp, #0xc0]
100df1a1c:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1a20:     	add	x8, sp, #0x420
100df1a24:     	ldur	q0, [x8, #0x8]
100df1a28:     	ldr	x9, [sp, #0x2f8]
100df1a2c:     	stur	q0, [x9, #0x8]
100df1a30:     	ldr	x8, [sp, #0x438]
100df1a34:     	str	x8, [x9, #0x18]
100df1a38:     	str	xzr, [x9]
100df1a3c:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df1a40:     	mov	w8, #0x1                ; =1
100df1a44:     	ldr	x9, [sp, #0x2f8]
100df1a48:     	strb	w8, [x9, #0x8]
100df1a4c:     	ldr	x10, [sp, #0x80]
100df1a50:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1a54:     	mov	w8, #0x1                ; =1
100df1a58:     	ldr	x9, [sp, #0x2f8]
100df1a5c:     	strb	w8, [x9, #0x8]
100df1a60:     	ldr	x10, [sp, #0x78]
100df1a64:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1a68:     	mov	w8, #0x1                ; =1
100df1a6c:     	ldr	x9, [sp, #0x2f8]
100df1a70:     	strb	w8, [x9, #0x8]
100df1a74:     	ldr	x10, [sp, #0x60]
100df1a78:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1a7c:     	mov	w8, #0x1                ; =1
100df1a80:     	ldr	x9, [sp, #0x2f8]
100df1a84:     	strb	w8, [x9, #0x8]
100df1a88:     	ldr	x10, [sp, #0x70]
100df1a8c:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1a90:     	sub	x0, x29, #0xa8
100df1a94:     	sub	x4, x29, #0x78
100df1a98:     	mov	x1, x20
100df1a9c:     	ldr	x2, [sp, #0x328]
100df1aa0:     	ldr	x3, [sp, #0x308]
100df1aa4:     	bl	0x100cf0c70 <__RNvMNtCsgTc3P8JF8aG_8otter_vm12iterator_opsNtB4_11Interpreter27async_generator_resume_next>
100df1aa8:     	ldur	x8, [x29, #-0xa8]
100df1aac:     	cbz	x8, 0x100df1400 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6e40>
100df1ab0:     	cmp	x8, #0x2
100df1ab4:     	b.ne	0x100df1ad4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7514>
100df1ab8:     	mov	w8, #0x17               ; =23
100df1abc:     	ldr	x9, [sp, #0x2f8]
100df1ac0:     	strb	w8, [x9, #0x8]
100df1ac4:     	mov	w8, #0xa                ; =10
100df1ac8:     	str	x8, [x9, #0x10]
100df1acc:     	str	xzr, [x9]
100df1ad0:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df1ad4:     	mov	x8, #0x0                ; =0
100df1ad8:     	sub	x9, x29, #0xa8
100df1adc:     	ldur	q0, [x9, #0x8]
100df1ae0:     	ldr	x10, [sp, #0x2f8]
100df1ae4:     	stur	q0, [x10, #0x8]
100df1ae8:     	ldur	x9, [x29, #-0x90]
100df1aec:     	b	0x100df1360 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6da0>
100df1af0:     	mov	w8, #0x1                ; =1
100df1af4:     	ldr	x9, [sp, #0x2f8]
100df1af8:     	strb	w8, [x9, #0x8]
100df1afc:     	ldr	x10, [sp, #0x68]
100df1b00:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1b04:     	mov	w8, #0x1                ; =1
100df1b08:     	ldr	x9, [sp, #0x2f8]
100df1b0c:     	strb	w8, [x9, #0x8]
100df1b10:     	ldr	x10, [sp, #0x58]
100df1b14:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1b18:     	mov	w8, #0x17               ; =23
100df1b1c:     	ldr	x9, [sp, #0x2f8]
100df1b20:     	strb	w8, [x9, #0x8]
100df1b24:     	str	x19, [x9, #0x10]
100df1b28:     	str	xzr, [x9]
100df1b2c:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df1b30:     	mov	w8, #0x1                ; =1
100df1b34:     	ldr	x9, [sp, #0x2f8]
100df1b38:     	strb	w8, [x9, #0x8]
100df1b3c:     	ldr	x10, [sp, #0x50]
100df1b40:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1b44:     	mov	x0, x2
100df1b48:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100df1b4c:     	add	x2, x2, #0x470
100df1b50:     	mov	x1, x28
100df1b54:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100df1b58:     	ldr	w8, [x0, #0x64]
100df1b5c:     	cbz	w8, 0x100df1b8c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x75cc>
100df1b60:     	ldr	x1, [x20, #0x7558]
100df1b64:     	sub	x23, x8, #0x1
100df1b68:     	cmp	x23, x1
100df1b6c:     	b.hs	0x100df1ea4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x78e4>
100df1b70:     	mov	x26, #0x0               ; =0
100df1b74:     	mov	x22, #0x0               ; =0
100df1b78:     	ldr	x8, [x20, #0x7550]
100df1b7c:     	mov	w9, #0xd0               ; =208
100df1b80:     	mul	x9, x23, x9
100df1b84:     	str	xzr, [x8, x9]
100df1b88:     	b	0x100df1b94 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x75d4>
100df1b8c:     	mov	x26, #0x0               ; =0
100df1b90:     	mov	x22, #0x0               ; =0
100df1b94:     	mov	w28, #0x2               ; =2
100df1b98:     	b	0x100df1d08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7748>
100df1b9c:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100df1ba0:     	add	x2, x2, #0x4e8
100df1ba4:     	ldr	x0, [sp, #0x328]
100df1ba8:     	mov	x1, x28
100df1bac:     	bl	0x10104c260 <__RNvXs5_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStackINtNtNtCsjIpoRzUjyHQ_4core3ops5index8IndexMutjE9index_mut>
100df1bb0:     	ldr	w8, [x0, #0x64]
100df1bb4:     	cbz	w8, 0x100df1bd8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7618>
100df1bb8:     	ldr	x1, [x20, #0x7558]
100df1bbc:     	sub	x23, x8, #0x1
100df1bc0:     	cmp	x23, x1
100df1bc4:     	b.hs	0x100df1ea4 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x78e4>
100df1bc8:     	ldr	x8, [x20, #0x7550]
100df1bcc:     	mov	w9, #0xd0               ; =208
100df1bd0:     	mul	x9, x23, x9
100df1bd4:     	str	xzr, [x8, x9]
100df1bd8:     	ldp	x28, x9, [x29, #-0xa0]
100df1bdc:     	lsr	x8, x9, #8
100df1be0:     	ldur	x10, [x29, #-0x90]
100df1be4:     	str	x10, [sp, #0x258]
100df1be8:     	mov	x23, x9
100df1bec:     	mov	x22, x28
100df1bf0:     	b	0x100df1d08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7748>
100df1bf4:     	mov	x22, #0x0               ; =0
100df1bf8:     	mov	x26, #0x0               ; =0
100df1bfc:     	mov	w28, #0x2               ; =2
100df1c00:     	str	x11, [sp, #0x258]
100df1c04:     	b	0x100df1d08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7748>
100df1c08:     	mov	x26, #0x0               ; =0
100df1c0c:     	ldp	x28, x9, [x29, #-0x78]
100df1c10:     	lsr	x8, x9, #8
100df1c14:     	ldur	x11, [x29, #-0x68]
100df1c18:     	mov	x22, x28
100df1c1c:     	mov	x23, x9
100df1c20:     	str	x11, [sp, #0x258]
100df1c24:     	b	0x100df1d08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7748>
100df1c28:     	mov	w8, #0x1                ; =1
100df1c2c:     	ldr	x9, [sp, #0x2f8]
100df1c30:     	strb	w8, [x9, #0x8]
100df1c34:     	ldr	x10, [sp, #0x30]
100df1c38:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1c3c:     	mov	w8, #0x1                ; =1
100df1c40:     	ldr	x9, [sp, #0x2f8]
100df1c44:     	strb	w8, [x9, #0x8]
100df1c48:     	ldr	x10, [sp, #0x28]
100df1c4c:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1c50:     	mov	x23, x6
100df1c54:     	mov	x21, x0
100df1c58:     	mov	x0, x4
100df1c5c:     	mov	x22, x19
100df1c60:     	mov	x19, x4
100df1c64:     	bl	0x101b64e08 <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm10native_abi5frameNtB5_5Frame20derive_logical_depth>
100df1c68:     	mov	x4, x19
100df1c6c:     	mov	x19, x22
100df1c70:     	mov	x24, x0
100df1c74:     	mov	x0, x21
100df1c78:     	mov	x6, x23
100df1c7c:     	ldr	x5, [sp, #0x328]
100df1c80:     	mov	x25, #0x0               ; =0
100df1c84:     	mov	w8, w24
100df1c88:     	str	x4, [sp, #0x358]
100df1c8c:     	cmp	x23, x8
100df1c90:     	b.lo	0x100dea7c8 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x208>
100df1c94:     	b	0x100dea764 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x1a4>
100df1c98:     	ldr	x11, [sp, #0x438]
100df1c9c:     	lsr	x8, x21, #8
100df1ca0:     	mov	x22, x28
100df1ca4:     	mov	x23, x21
100df1ca8:     	str	x11, [sp, #0x258]
100df1cac:     	b	0x100df1d08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7748>
100df1cb0:     	ldrb	w8, [sp, #0x437]
100df1cb4:     	add	x10, sp, #0x420
100df1cb8:     	ldurh	w9, [x10, #0x15]
100df1cbc:     	orr	w8, w9, w8, lsl #16
100df1cc0:     	ldur	w9, [x10, #0x11]
100df1cc4:     	orr	x8, x9, x8, lsl #32
100df1cc8:     	ldr	x11, [sp, #0x438]
100df1ccc:     	mov	x28, x19
100df1cd0:     	mov	x22, x19
100df1cd4:     	mov	x23, x21
100df1cd8:     	str	x11, [sp, #0x258]
100df1cdc:     	b	0x100df1d08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7748>
100df1ce0:     	mov	x22, #0x0               ; =0
100df1ce4:     	mov	w28, #0x1               ; =1
100df1ce8:     	mov	w26, #0x1               ; =1
100df1cec:     	str	x11, [sp, #0x258]
100df1cf0:     	b	0x100df1d08 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7748>
100df1cf4:     	mov	x22, #0x0               ; =0
100df1cf8:     	mov	w28, #0x1               ; =1
100df1cfc:     	mov	w26, #0x1               ; =1
100df1d00:     	str	x11, [sp, #0x258]
100df1d04:     	mov	x23, x10
100df1d08:     	bfxil	x22, x28, #0, #8
100df1d0c:     	bfi	x23, x8, #8, #56
100df1d10:     	ldr	x0, [sp, #0x260]
100df1d14:     	bl	0x100ce87c0 <__RNvMNtCsgTc3P8JF8aG_8otter_vm12iterator_opsNtB4_11Interpreter18iterator_mark_done>
100df1d18:     	ldr	x9, [sp, #0x2f8]
100df1d1c:     	stp	x22, x23, [x9, #0x8]
100df1d20:     	ldr	x8, [sp, #0x258]
100df1d24:     	str	x8, [x9, #0x18]
100df1d28:     	tbz	w26, #0x0, 0x100df1d34 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7774>
100df1d2c:     	str	xzr, [x9]
100df1d30:     	b	0x100df1d3c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x777c>
100df1d34:     	mov	w8, #0x1                ; =1
100df1d38:     	str	x8, [x9]
100df1d3c:     	ldr	x9, [sp, #0x2e8]
100df1d40:     	ldr	x8, [x9]
100df1d44:     	ldr	x10, [sp, #0x350]
100df1d48:     	add	x8, x8, x10
100df1d4c:     	str	x8, [x9]
100df1d50:     	ldr	w11, [sp, #0x2f4]
100df1d54:     	ldr	x10, [sp, #0x338]
100df1d58:     	cbz	x10, 0x100df1d70 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x77b0>
100df1d5c:     	ldr	x9, [sp, #0x2e0]
100df1d60:     	ldr	x8, [x9, #0x10]
100df1d64:     	adds	x8, x8, x10
100df1d68:     	csinv	x8, x8, xzr, lo
100df1d6c:     	str	x8, [x9, #0x10]
100df1d70:     	tbz	w11, #0x0, 0x100df1dbc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x77fc>
100df1d74:     	ldr	x8, [sp, #0x370]
100df1d78:     	cbz	x8, 0x100df1dbc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x77fc>
100df1d7c:     	mov	x19, #-0x1              ; =-1
100df1d80:     	ldaddl	x19, x8, [x8]
100df1d84:     	cmp	x8, #0x1
100df1d88:     	b.ne	0x100df1d9c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x77dc>
100df1d8c:     	dmb	ishld
100df1d90:     	add	x20, sp, #0x370
100df1d94:     	add	x0, sp, #0x370
100df1d98:     	bl	0x100e5ab08 <__RNvMsn_NtCs6zY7rJBZy9D_5alloc4syncINtB5_3ArcNtNtCsgTc3P8JF8aG_8otter_vm10code_space12ChunkPayloadE9drop_slowBK_>
100df1d9c:     	ldr	x8, [sp, #0x378]
100df1da0:     	ldaddl	x19, x8, [x8]
100df1da4:     	cmp	x8, #0x1
100df1da8:     	b.ne	0x100df1dbc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x77fc>
100df1dac:     	add	x8, sp, #0x370
100df1db0:     	dmb	ishld
100df1db4:     	add	x0, x8, #0x8
100df1db8:     	bl	0x100e5aca0 <__RNvMsn_NtCs6zY7rJBZy9D_5alloc4syncINtB5_3ArcNtNtCsgTc3P8JF8aG_8otter_vm10code_space9CodeSpaceE9drop_slowBK_>
100df1dbc:     	add	sp, sp, #0x580
100df1dc0:     	ldp	x29, x30, [sp, #0x50]
100df1dc4:     	ldp	x20, x19, [sp, #0x40]
100df1dc8:     	ldp	x22, x21, [sp, #0x30]
100df1dcc:     	ldp	x24, x23, [sp, #0x20]
100df1dd0:     	ldp	x26, x25, [sp, #0x10]
100df1dd4:     	ldp	x28, x27, [sp], #0x60
100df1dd8:     	ret
100df1ddc:     	adrp	x2, 0x103456000 <_anon.7a775211447c4b11c21165a1336b249e.3407+0x328>
100df1de0:     	add	x2, x2, #0x9a0
100df1de4:     	mov	w1, #0x2b               ; =43
100df1de8:     	adrp	x0, 0x102d17000 <_anon.7a775211447c4b11c21165a1336b249e.3406+0x283>
100df1dec:     	add	x0, x0, #0x201
100df1df0:     	bl	0x101af617c <__RNvNtCsjIpoRzUjyHQ_4core6option13expect_failed>
100df1df4:     	b	0x100df1ebc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x78fc>
100df1df8:     	mov	w8, #0x1                ; =1
100df1dfc:     	ldr	x9, [sp, #0x2f8]
100df1e00:     	strb	w8, [x9, #0x8]
100df1e04:     	ldr	x10, [sp, #0x20]
100df1e08:     	b	0x100df125c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6c9c>
100df1e0c:     	mov	w8, #0x1                ; =1
100df1e10:     	ldr	x9, [sp, #0x2f8]
100df1e14:     	strb	w8, [x9, #0x8]
100df1e18:     	b	0x100df1578 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6fb8>
100df1e1c:     	ldr	x0, [sp, #0x260]
100df1e20:     	bl	0x100ce87c0 <__RNvMNtCsgTc3P8JF8aG_8otter_vm12iterator_opsNtB4_11Interpreter18iterator_mark_done>
100df1e24:     	ldur	q0, [x29, #-0xd8]
100df1e28:     	ldr	x9, [sp, #0x2f8]
100df1e2c:     	stur	q0, [x9, #0x8]
100df1e30:     	ldur	x8, [x29, #-0xc8]
100df1e34:     	b	0x100df0e34 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x6874>
100df1e38:     	adrp	x0, 0x102d17000 <_anon.7a775211447c4b11c21165a1336b249e.3406+0x283>
100df1e3c:     	add	x0, x0, #0x22c
100df1e40:     	adrp	x2, 0x103456000 <_anon.7a775211447c4b11c21165a1336b249e.3407+0x328>
100df1e44:     	add	x2, x2, #0x9d0
100df1e48:     	mov	w1, #0x7f               ; =127
100df1e4c:     	bl	0x101af63b8 <__RNvNtCsjIpoRzUjyHQ_4core9panicking9panic_fmt>
100df1e50:     	b	0x100df1ebc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x78fc>
100df1e54:     	adrp	x2, 0x103456000 <_anon.7a775211447c4b11c21165a1336b249e.3407+0x328>
100df1e58:     	add	x2, x2, #0x9b8
100df1e5c:     	b	0x100df1e74 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x78b4>
100df1e60:     	adrp	x2, 0x103456000 <_anon.7a775211447c4b11c21165a1336b249e.3407+0x328>
100df1e64:     	add	x2, x2, #0x9e8
100df1e68:     	b	0x100df1e74 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x78b4>
100df1e6c:     	adrp	x2, 0x103456000 <_anon.7a775211447c4b11c21165a1336b249e.3407+0x328>
100df1e70:     	add	x2, x2, #0xa00
100df1e74:     	mov	w1, #0xd                ; =13
100df1e78:     	adrp	x0, 0x102d15000 <_anon.7a775211447c4b11c21165a1336b249e.2316+0x608>
100df1e7c:     	add	x0, x0, #0x7c7
100df1e80:     	b	0x100df1df0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7830>
100df1e84:     	adrp	x2, 0x103449000 <_anon.7a775211447c4b11c21165a1336b249e.696+0xb10>
100df1e88:     	add	x2, x2, #0x430
100df1e8c:     	mov	x0, x23
100df1e90:     	bl	0x101af6248 <__RNvNtCsjIpoRzUjyHQ_4core9panicking18panic_bounds_check>
100df1e94:     	b	0x100df1ebc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x78fc>
100df1e98:     	adrp	x2, 0x103445000 <_anon.49087316163ed4345739c0711faf6653.2138+0x8d8>
100df1e9c:     	add	x2, x2, #0xd10
100df1ea0:     	b	0x100df1e8c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x78cc>
100df1ea4:     	adrp	x2, 0x103449000 <_anon.7a775211447c4b11c21165a1336b249e.696+0xb10>
100df1ea8:     	add	x2, x2, #0x448
100df1eac:     	b	0x100df1e8c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x78cc>
100df1eb0:     	mov	w0, #0x1                ; =1
100df1eb4:     	mov	w1, #0x29               ; =41
100df1eb8:     	bl	0x101af5320 <__RNvNtCs6zY7rJBZy9D_5alloc7raw_vec12handle_error>
100df1ebc:     	brk	#0x1
100df1ec0:     	b	0x100df1fcc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a0c>
100df1ec4:     	b	0x100df1fcc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a0c>
100df1ec8:     	b	0x100df1ecc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x790c>
100df1ecc:     	mov	x19, x0
100df1ed0:     	mov	x0, x27
100df1ed4:     	bl	0x100c45d00 <__RINvNtCsjIpoRzUjyHQ_4core3ptr9drop_glueINtNtB4_6option6OptionINtNtCs6zY7rJBZy9D_5alloc5boxed3BoxNtNtCsgTc3P8JF8aG_8otter_vm10cold_frame9ColdFrameEEEB1A_>
100df1ed8:     	b	0x100df1fd0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a10>
100df1edc:     	b	0x100df1fcc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a0c>
100df1ee0:     	b	0x100df1fcc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a0c>
100df1ee4:     	mov	x19, x0
100df1ee8:     	mov	x0, x25
100df1eec:     	bl	0x100c45d00 <__RINvNtCsjIpoRzUjyHQ_4core3ptr9drop_glueINtNtB4_6option6OptionINtNtCs6zY7rJBZy9D_5alloc5boxed3BoxNtNtCsgTc3P8JF8aG_8otter_vm10cold_frame9ColdFrameEEEB1A_>
100df1ef0:     	b	0x100df1fd0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a10>
100df1ef4:     	b	0x100df1fcc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a0c>
100df1ef8:     	mov	x19, x0
100df1efc:     	add	x0, sp, #0x3f8
100df1f00:     	bl	0x100c45ddc <__RINvNtCsjIpoRzUjyHQ_4core3ptr9drop_glueINtNtB4_6option6OptionNtCsgTc3P8JF8aG_8otter_vm10MethodSiteEEBZ_>
100df1f04:     	b	0x100df1fd0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a10>
100df1f08:     	mov	x19, x0
100df1f0c:     	add	x0, sp, #0x420
100df1f10:     	bl	0x100c595a8 <__RINvNtCsjIpoRzUjyHQ_4core3ptr9drop_glueNtNtCsgTc3P8JF8aG_8otter_vm17execution_context16ExecutionContextEBF_>
100df1f14:     	b	0x100df2014 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a54>
100df1f18:     	mov	x19, x0
100df1f1c:     	ldr	x8, [sp, #0x378]
100df1f20:     	mov	x9, #-0x1               ; =-1
100df1f24:     	ldaddl	x9, x8, [x8]
100df1f28:     	cmp	x8, #0x1
100df1f2c:     	b.ne	0x100df2014 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a54>
100df1f30:     	dmb	ishld
100df1f34:     	add	x0, x20, #0x8
100df1f38:     	bl	0x100e5aca0 <__RNvMsn_NtCs6zY7rJBZy9D_5alloc4syncINtB5_3ArcNtNtCsgTc3P8JF8aG_8otter_vm10code_space9CodeSpaceE9drop_slowBK_>
100df1f3c:     	b	0x100df2014 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a54>
100df1f40:     	bl	0x101af6230 <__RNvNtCsjIpoRzUjyHQ_4core9panicking16panic_in_cleanup>
100df1f44:     	mov	x19, x0
100df1f48:     	ldr	x8, [x20, #0x7680]
100df1f4c:     	cbz	x8, 0x100df1f58 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7998>
100df1f50:     	ldr	x0, [x20, #0x7688]
100df1f54:     	bl	0x101baa2b4 <_mi_free>
100df1f58:     	add	x8, sp, #0x2c9
100df1f5c:     	ldur	q0, [x8, #0xff]
100df1f60:     	add	x8, sp, #0x2d9
100df1f64:     	ldur	q1, [x8, #0xff]
100df1f68:     	ldr	x8, [sp, #0x300]
100df1f6c:     	stp	q0, q1, [x8, #0x40]
100df1f70:     	add	x9, sp, #0x2e9
100df1f74:     	ldur	q0, [x9, #0xff]
100df1f78:     	str	q0, [x8, #0x60]
100df1f7c:     	add	x9, sp, #0x289
100df1f80:     	ldur	q0, [x9, #0xff]
100df1f84:     	add	x9, sp, #0x299
100df1f88:     	ldur	q1, [x9, #0xff]
100df1f8c:     	stp	q0, q1, [x8]
100df1f90:     	add	x9, sp, #0x2a9
100df1f94:     	ldur	q0, [x9, #0xff]
100df1f98:     	add	x9, sp, #0x2b9
100df1f9c:     	ldur	q1, [x9, #0xff]
100df1fa0:     	stp	q0, q1, [x8, #0x20]
100df1fa4:     	b	0x100df1fd0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a10>
100df1fa8:     	mov	x19, x0
100df1fac:     	tbnz	w21, #0x0, 0x100df200c <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a4c>
100df1fb0:     	b	0x100df2014 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a54>
100df1fb4:     	b	0x100df1fcc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a0c>
100df1fb8:     	mov	x19, x0
100df1fbc:     	add	x0, sp, #0x388
100df1fc0:     	bl	0x100c573d8 <__RINvNtCsjIpoRzUjyHQ_4core3ptr9drop_glueNtNtCsgTc3P8JF8aG_8otter_vm11cpu_profile11CpuProfilerEBF_>
100df1fc4:     	b	0x100df1fd0 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a10>
100df1fc8:     	b	0x100df1fcc <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a0c>
100df1fcc:     	mov	x19, x0
100df1fd0:     	ldr	x9, [sp, #0x2e8]
100df1fd4:     	ldr	x8, [x9]
100df1fd8:     	ldr	x10, [sp, #0x350]
100df1fdc:     	add	x8, x8, x10
100df1fe0:     	str	x8, [x9]
100df1fe4:     	ldr	x8, [sp, #0x338]
100df1fe8:     	cbz	x8, 0x100df2004 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a44>
100df1fec:     	ldr	x9, [sp, #0x2e0]
100df1ff0:     	ldr	x8, [x9, #0x10]
100df1ff4:     	ldr	x10, [sp, #0x338]
100df1ff8:     	adds	x8, x8, x10
100df1ffc:     	csinv	x8, x8, xzr, lo
100df2000:     	str	x8, [x9, #0x10]
100df2004:     	ldr	w8, [sp, #0x2f4]
100df2008:     	tbz	w8, #0x0, 0x100df2014 <__RNvMs1_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks+0x7a54>
100df200c:     	add	x0, sp, #0x370
100df2010:     	bl	0x100c4629c <__RINvNtCsjIpoRzUjyHQ_4core3ptr9drop_glueINtNtB4_6option6OptionNtNtCsgTc3P8JF8aG_8otter_vm17execution_context16ExecutionContextEEB11_>
100df2014:     	mov	x0, x19
100df2018:     	bl	0x101bb1e20 <dyld_stub_binder+0x101bb1e20>
100df201c:     	bl	0x101af6230 <__RNvNtCsjIpoRzUjyHQ_4core9panicking16panic_in_cleanup>
