//! Terminal compiler acknowledgements and deferred source reclamation.
//!
//! # Contents
//! - Delayed Template and optimizing workers retain real evictable sources.
//! - The first pinned turn defers reclamation; the next after ACK retries it.
//!
//! # Invariants
//! - Background compilation stays enabled and the worker gate controls ordering.
//! - ACK releases source leases without collecting or publishing cancelled code.
//! - Reused pinned turns add no source growth that could mask a stale watermark.
//!
//! # See also
//! - `jit_compile_queue` owns cancellation and terminal acknowledgement.
//! - Parent fixtures own the common source-work admission helper.

use super::*;

#[test]
fn cancelled_background_compiles_rearm_code_reclamation_after_terminal_ack() {
    #[derive(Debug)]
    struct DelayedCompiler {
        entered: std::sync::mpsc::Sender<()>,
        release: Mutex<()>,
    }

    impl jit::JitCompilerHook for DelayedCompiler {
        fn optimizing_tier_enabled(&self) -> bool {
            true
        }

        fn compile_function(
            &self,
            request: jit::JitCompileRequest,
        ) -> Result<jit::JitCompileStatus, jit::JitCompileError> {
            self.entered.send(()).unwrap();
            drop(self.release.lock().unwrap());
            assert!(request.cancellation.is_cancelled());
            Ok(jit::JitCompileStatus::Cancelled)
        }

        fn compile_optimized_function(
            &self,
            request: jit::JitCompileRequest,
        ) -> Result<jit::JitCompileStatus, jit::JitCompileError> {
            self.compile_function(request)
        }
    }

    for tier in [
        crate::tier_policy::CostedTier::Template,
        crate::tier_policy::CostedTier::Optimizing,
    ] {
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        vm.set_jit_compiler_threads(1);
        vm.set_code_eviction_high_water_bytes(0);
        let source = vm
            .link_evictable_module(
                crate::test_support::minimal_bytecode_module("delayed-source.js"),
                crate::source_registry::SourceRegistry::default(),
            )
            .unwrap();
        let turn = vm
            .link_module(
                crate::test_support::minimal_bytecode_module("reclamation-turn.js"),
                crate::source_registry::SourceRegistry::default(),
            )
            .unwrap();
        let fid = source.function_base();
        let payload = vm.code_eviction_stats().retained_bytes;
        assert!(payload > 0);
        let (entered, entry) = std::sync::mpsc::channel();
        let hook = Arc::new(DelayedCompiler {
            entered,
            release: Mutex::new(()),
        });
        vm.jit_hook = Some(hook.clone());
        let active_worker = hook.release.lock().unwrap();
        fund_compile_work(&mut vm, &source, fid, tier);
        match tier {
            crate::tier_policy::CostedTier::Template => assert!(matches!(
                vm.compile_jit_function(&source, fid, None),
                TemplateCompileOutcome::Deferred
            )),
            crate::tier_policy::CostedTier::Optimizing => assert!(
                vm.compile_optimized_jit_function(&source, fid, None)
                    .is_none()
            ),
        }
        entry
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("background compiler entered its hook");
        drop(source);

        assert!(vm.run(&turn).unwrap().is_undefined());
        assert_eq!(vm.jit_compile_queue.pending_count(), 1);
        let deferred = vm.code_eviction_stats();
        assert_eq!(deferred.census_passes, 1);
        assert_eq!(deferred.evicted_chunks, 0);
        assert_eq!(deferred.retained_bytes, payload);
        assert!(vm.code_space.eviction_candidates().is_empty());
        assert!(vm.code_eviction_trigger_bytes > payload);

        drop(active_worker);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !vm.jit_compile_queue.has_finished() {
            assert!(std::time::Instant::now() < deadline, "worker terminal ACK");
            std::thread::yield_now();
        }
        vm.install_finished_optimized_compiles();
        assert_eq!(vm.jit_compile_queue.pending_count(), 0);
        assert_eq!(vm.code_space.eviction_candidates().len(), 1);
        assert_eq!(vm.code_eviction_stats().retained_bytes, payload);
        assert!(!vm.jit_code.contains_key(&fid));
        assert!(!vm.jit_optimized_code.contains_key(&fid));

        // Reuse the pinned turn: no new source or payload growth can
        // accidentally cross the watermark left by the deferred census.
        assert!(vm.run(&turn).unwrap().is_undefined());
        let reclaimed = vm.code_eviction_stats();
        assert_eq!(reclaimed.census_passes, deferred.census_passes + 1);
        assert_eq!(reclaimed.evicted_chunks, 1, "{tier:?} terminal ACK retry");
        assert_eq!(reclaimed.evicted_bytes, payload);
        assert_eq!(reclaimed.retained_bytes, 0);
    }
}
