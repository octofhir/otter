//! Macro-generated JS surface benchmark parity checks.
//!
//! These benches compare Task 97 macro-generated static specs against
//! equivalent handwritten Task 96 specs. The measured operation is the
//! mutator-bound builder install path; generated code should be the same
//! shape as handwritten specs, so benchmark deltas should stay within
//! normal Criterion noise.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use otter_macros::{js_class, js_namespace, raft};
use otter_vm::{
    AccessorSpec, Attr, ClassBuilder, ClassSpec, ConstructorSpec, Interpreter, MethodSpec,
    NamespaceBuilder, NamespaceSpec, NativeCall,
};

static HANDWRITTEN_NAMESPACE_METHODS: &[MethodSpec] = &[
    MethodSpec {
        name: "one",
        length: 0,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(macro_namespace::one),
    },
    MethodSpec {
        name: "two",
        length: 1,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(macro_namespace::two),
    },
];

static HANDWRITTEN_NAMESPACE_SPEC: NamespaceSpec = NamespaceSpec {
    name: "BenchNs",
    methods: HANDWRITTEN_NAMESPACE_METHODS,
    accessors: &[],
    constants: &[],
    attrs: Attr::global_binding(),
};

#[js_namespace(name = "BenchNs", spec = MACRO_NAMESPACE_SPEC)]
mod macro_namespace {
    use otter_vm::{NativeCtx, NativeError, Value};

    #[js_fn(name = "one", length = 0)]
    pub fn one(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::number_i32(1))
    }

    #[js_fn(name = "two", length = 1)]
    pub fn two(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::number_i32(2))
    }
}

mod raft_namespace {
    use otter_vm::{NativeCtx, NativeError, Value};

    pub fn one(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::number_i32(1))
    }

    pub fn two(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::number_i32(2))
    }
}

raft! {
    static RAFT_NAMESPACE_SPEC: namespace("BenchNs") {
        methods: [
            "one" => raft_namespace::one, length = 0;
            "two" => raft_namespace::two, length = 1;
        ]
    }
}

static HANDWRITTEN_CLASS_STATIC_METHODS: &[MethodSpec] = &[MethodSpec {
    name: "from",
    length: 1,
    attrs: Attr::builtin_function(),
    call: NativeCall::Static(macro_class::from),
}];

static HANDWRITTEN_CLASS_PROTOTYPE_METHODS: &[MethodSpec] = &[MethodSpec {
    name: "valueOf",
    length: 0,
    attrs: Attr::builtin_function(),
    call: NativeCall::Static(macro_class::value_of),
}];

static HANDWRITTEN_CLASS_PROTOTYPE_ACCESSORS: &[AccessorSpec] = &[AccessorSpec {
    name: "answer",
    get: Some(NativeCall::Static(macro_class::get_answer)),
    set: Some(NativeCall::Static(macro_class::set_answer)),
    attrs: Attr::new(false, false, true),
}];

static HANDWRITTEN_CLASS_SPEC: ClassSpec = ClassSpec {
    constructor: ConstructorSpec {
        name: "BenchClass",
        length: 1,
        call: NativeCall::Static(macro_class::construct),
        static_methods: HANDWRITTEN_CLASS_STATIC_METHODS,
        prototype_methods: HANDWRITTEN_CLASS_PROTOTYPE_METHODS,
        attrs: Attr::global_binding(),
    },
    prototype_accessors: HANDWRITTEN_CLASS_PROTOTYPE_ACCESSORS,
};

#[js_class(name = "BenchClass", spec = MACRO_CLASS_SPEC)]
mod macro_class {
    use otter_vm::{NativeCtx, NativeError, Value};

    #[js_constructor(length = 1)]
    pub fn construct(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::undefined())
    }

    #[js_static_method(name = "from", length = 1)]
    pub fn from(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::number_i32(1))
    }

    #[js_method(name = "valueOf", length = 0)]
    pub fn value_of(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::number_i32(2))
    }

    #[js_getter(name = "answer")]
    pub fn get_answer(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::number_i32(42))
    }

    #[js_setter(name = "answer")]
    pub fn set_answer(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::undefined())
    }
}

fn install_namespace(spec: &'static NamespaceSpec) {
    let mut interp = Interpreter::new();
    let namespace = NamespaceBuilder::from_spec(interp.gc_heap_mut(), spec)
        .expect("namespace builder")
        .build()
        .expect("namespace build");
    std::hint::black_box(namespace);
}

fn install_class(spec: &'static ClassSpec) {
    let mut interp = Interpreter::new();
    let class = ClassBuilder::from_spec(interp.gc_heap_mut(), spec)
        .build()
        .expect("class build");
    std::hint::black_box(class);
}

fn bench_js_surface_macros(c: &mut Criterion) {
    let mut namespace = c.benchmark_group("js_surface_namespace_install");
    namespace.bench_function("handwritten", |b| {
        b.iter_batched(
            || (),
            |()| install_namespace(&HANDWRITTEN_NAMESPACE_SPEC),
            BatchSize::SmallInput,
        );
    });
    namespace.bench_function("js_namespace_macro", |b| {
        b.iter_batched(
            || (),
            |()| install_namespace(&MACRO_NAMESPACE_SPEC),
            BatchSize::SmallInput,
        );
    });
    namespace.bench_function("raft_macro", |b| {
        b.iter_batched(
            || (),
            |()| install_namespace(&RAFT_NAMESPACE_SPEC),
            BatchSize::SmallInput,
        );
    });
    namespace.finish();

    let mut class = c.benchmark_group("js_surface_class_install");
    class.bench_function("handwritten", |b| {
        b.iter_batched(
            || (),
            |()| install_class(&HANDWRITTEN_CLASS_SPEC),
            BatchSize::SmallInput,
        );
    });
    class.bench_function("js_class_macro", |b| {
        b.iter_batched(
            || (),
            |()| install_class(&MACRO_CLASS_SPEC),
            BatchSize::SmallInput,
        );
    });
    class.finish();
}

criterion_group!(benches, bench_js_surface_macros);
criterion_main!(benches);
