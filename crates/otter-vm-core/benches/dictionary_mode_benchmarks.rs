//! Dictionary Mode Performance Benchmarks
//!
//! Compares property access performance between shape-based storage and dictionary mode.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use otter_vm_core::{GcRef, JsObject, MemoryManager, PropertyKey, value::Value};
use std::sync::Arc;

/// Benchmark: Shape-based storage property access (< 32 properties)
fn bench_shape_based_access(c: &mut Criterion) {
    let memory_manager = Arc::new(MemoryManager::test());

    c.bench_function("shape_based_set_20_props", |b| {
        b.iter(|| {
            let obj = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
            for i in 0..20 {
                let key = PropertyKey::string(&format!("prop{}", i));
                let _ = obj.set(key, Value::int32(i as i32));
            }
            black_box(obj)
        });
    });

    c.bench_function("shape_based_get_20_props", |b| {
        let obj = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        for i in 0..20 {
            let key = PropertyKey::string(&format!("prop{}", i));
            obj.set(key, Value::int32(i as i32));
        }

        b.iter(|| {
            let mut sum = 0i32;
            for i in 0..20 {
                let key = PropertyKey::string(&format!("prop{}", i));
                if let Some(v) = obj.get(&key) {
                    sum += v.as_int32().unwrap_or(0);
                }
            }
            black_box(sum)
        });
    });
}

/// Benchmark: Dictionary mode property access (> 32 properties)
fn bench_dictionary_mode_access(c: &mut Criterion) {
    let memory_manager = Arc::new(MemoryManager::test());

    c.bench_function("dictionary_set_50_props", |b| {
        b.iter(|| {
            let obj = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
            for i in 0..50 {
                let key = PropertyKey::string(&format!("prop{}", i));
                let _ = obj.set(key, Value::int32(i as i32));
            }
            black_box(obj)
        });
    });

    c.bench_function("dictionary_get_50_props", |b| {
        let obj = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        // Create 50 properties to trigger dictionary mode
        for i in 0..50 {
            let key = PropertyKey::string(&format!("prop{}", i));
            obj.set(key, Value::int32(i as i32));
        }

        // Verify it's in dictionary mode
        assert!(obj.is_dictionary_mode(), "Object should be in dictionary mode");

        b.iter(|| {
            let mut sum = 0i32;
            for i in 0..50 {
                let key = PropertyKey::string(&format!("prop{}", i));
                if let Some(v) = obj.get(&key) {
                    sum += v.as_int32().unwrap_or(0);
                }
            }
            black_box(sum)
        });
    });
}

/// Benchmark: Access after delete (triggers dictionary mode)
fn bench_delete_triggered_dictionary(c: &mut Criterion) {
    let memory_manager = Arc::new(MemoryManager::test());

    c.bench_function("post_delete_get_10_props", |b| {
        let obj = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        // Create 10 properties
        for i in 0..10 {
            let key = PropertyKey::string(&format!("prop{}", i));
            obj.set(key, Value::int32(i as i32));
        }
        // Delete one to trigger dictionary mode
        obj.delete(&PropertyKey::string("prop5"));

        assert!(obj.is_dictionary_mode(), "Object should be in dictionary mode after delete");

        b.iter(|| {
            let mut sum = 0i32;
            for i in 0..10 {
                if i == 5 { continue; }
                let key = PropertyKey::string(&format!("prop{}", i));
                if let Some(v) = obj.get(&key) {
                    sum += v.as_int32().unwrap_or(0);
                }
            }
            black_box(sum)
        });
    });
}

/// Benchmark: Compare same number of properties, shape vs dictionary
fn bench_compare_storage_modes(c: &mut Criterion) {
    let memory_manager = Arc::new(MemoryManager::test());

    // Shape-based: 25 properties (under threshold)
    c.bench_function("compare_shape_25_get", |b| {
        let obj = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        for i in 0..25 {
            let key = PropertyKey::string(&format!("p{}", i));
            obj.set(key, Value::int32(i as i32));
        }
        assert!(!obj.is_dictionary_mode());

        b.iter(|| {
            let mut sum = 0i32;
            for i in 0..25 {
                let key = PropertyKey::string(&format!("p{}", i));
                if let Some(v) = obj.get(&key) {
                    sum += v.as_int32().unwrap_or(0);
                }
            }
            black_box(sum)
        });
    });

    // Dictionary mode: 25 properties (forced via delete)
    c.bench_function("compare_dict_25_get", |b| {
        let obj = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        for i in 0..26 {
            let key = PropertyKey::string(&format!("p{}", i));
            obj.set(key, Value::int32(i as i32));
        }
        // Delete one to trigger dictionary and keep 25 props
        obj.delete(&PropertyKey::string("p25"));
        assert!(obj.is_dictionary_mode());

        b.iter(|| {
            let mut sum = 0i32;
            for i in 0..25 {
                let key = PropertyKey::string(&format!("p{}", i));
                if let Some(v) = obj.get(&key) {
                    sum += v.as_int32().unwrap_or(0);
                }
            }
            black_box(sum)
        });
    });
}

criterion_group!(
    benches,
    bench_shape_based_access,
    bench_dictionary_mode_access,
    bench_delete_triggered_dictionary,
    bench_compare_storage_modes
);
criterion_main!(benches);
