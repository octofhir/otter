# Emitted contract и удалённая работа

На object boundary producer передаёт операцию в x3 (ARM64) или rcx (x86 System V). Общий C emitter владеет Microsoft shadow space и hidden pair result; operation попадает в существующий outgoing word layout. Private JS ABI не меняется.

Единственный `ObjectProtocolValueOp` владеет словами 0..21; неизвестные и high-bit слова отвергаются до bind/semantics. У Graph операция следует SSA Kind, включая rewritten inequality и nullish operand, независимо от исходного opcode. Unit emitted proof сохраняет inline source function 501 / PC 0, outer call PC 0, canonical homes и exception scratch на ARM64/x86.

Runtime method 111→22 static instructions; old published-opcode selector 149 static plus callees удалён именно из object operation path. Actual new outer stub 109 static instructions включает method inline body; 22 к 109 не прибавляются. Full-word check и decode — пять emitted ARM64 instructions на допустимом пути, producer — один MOV. Bind, kernel roots, counters, status/exception completion сохранены.

Runtime self profile, 3 runs before/after: 3018→2983 total; object physical boundary 205→109, selector subtree 104→0. Старые 200 named-stub samples дополнены 5 внутри exact method range. Новые 109 включают 101 inline-attributed samples внутри точного `[0x1007f6578,0x1007f672c)`: подсчёт только по имени ошибочно дал бы 8. Inclusive не суммируется; failed parallel recorder outputs исключены.

Generated Box2d: 475 matching bundles с идентичными root/source records; у 224 optimizing bundles IR и deopt идентичны. У 446 assembly идентична. В 29 bundles 100 object calls дают +400 operation bytes и +40 pool alignment bytes. c369/f1182 v136 сохраняет record 4294967291 со spillSlot[0..6,13] и logicalPC 98 / bytePC 1033; добавляет MOV x3,#4 на 0x1e64. BLR и return associations смещаются с code layout, сохраняя прежние ids; completion route не меняется. Hot f1228/f1230 без object calls assembly идентична.

Финальный fixed-work: Box2d 14,191,408,781→13,553,365,200 retired instructions (−4.495985%). Это whole-launch fixed work, включая compilation/GC; exact dynamic per-transition cost из sampling не выводится. Все 13 бенчей и все положительные дельты записаны в summary/performance/gate-summary. Причина малых общих прибавок и оставшихся regexp/Splay regressions не установлена.

Detailed local captures: before/after-runtime-asm.*, runtime-profile-comparison.json, generated-operation-evidence.json, before-runtime-profile/, after-runtime-profile-sequential/, fixed-work-idle-final/, performance-under-load.json, fixed-work-repeats.json и gc-diagnostics.json. Старые CLI binaries не сохранялись; user hot_asm.py не изменён.
