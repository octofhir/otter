//! Moving-GC invariants for module metadata and Error result builders.
//!
//! # Contents
//! - `import.meta` null-prototype object and URL publication.
//! - Error/AggregateError prototype, descriptor, and identity retention.
//! - One-shot construct-prototype lookup, fallback provenance, and receiver
//!   reuse versus fresh ordinary-call allocation.
//! - Direct derived `super` dispatch through a bound native constructor.
//! - Direct Proxy call/construct trap lookup and argument-array publication.
//! - Observable constructor ordering and abrupt completion cutoffs.
//! - AggregateError iteration through an array's overridden `@@iterator`.
//!
//! # Invariants
//! - A result receiver is held in one canonical handle scope before any later
//!   string, property-shape, callback, or array allocation can move it.
//! - OrdinaryCreateFromConstructor precedes message coercion; Error cause
//!   installation precedes AggregateError iteration and `errors` publication.
//! - Construct dispatch reads `new.target.prototype` once and retains explicit
//!   object-like values; ordinary `.call` never mutates its supplied receiver.
//! - AggregateError streams iterator values into its final rooted array without
//!   a raw `Vec<Value>` snapshot.

use std::path::Path;

use otter_runtime::{JitSelection, Otter, Runtime, SourceInput};

fn run_script(selection: JitSelection, source: &str, name: &str) -> String {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .build()
        .expect("runtime");
    runtime
        .run_script(SourceInput::from_javascript(source.to_string()), name)
        .expect("GC builder fixture")
        .completion_string()
        .to_owned()
}

fn assert_interpreter_and_template(source: &str, name: &str, expected: &str) {
    let interpreter = run_script(JitSelection::InterpreterOnly, source, name);
    let template = run_script(JitSelection::Template, source, name);
    assert_eq!(template, interpreter, "{name}: tier mismatch");
    assert_eq!(template, expected, "{name}: unexpected completion");
}

fn run_module(path: &Path) {
    Otter::new()
        .blocking_run_file(path)
        .expect("import.meta GC fixture");
}

#[test]
fn import_meta_object_and_url_survive_each_allocation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("main.mjs");
    std::fs::write(
        &entry,
        r#"
        function churn(seed) {
            let tail = null;
            for (let i = 0; i < 32; i++) {
                tail = { seed, i, text: "meta-" + seed + "-" + i, tail };
            }
            return tail;
        }

        const meta = import.meta;
        const held = meta;
        const allocated = churn(71);
        const descriptor = Object.getOwnPropertyDescriptor(meta, "url");
        if (
            meta !== held ||
            Object.getPrototypeOf(meta) !== null ||
            Reflect.ownKeys(meta).join(",") !== "url" ||
            typeof meta.url !== "string" ||
            meta.url.indexOf("main.mjs") < 0 ||
            descriptor.value !== meta.url ||
            descriptor.writable !== true ||
            descriptor.enumerable !== true ||
            descriptor.configurable !== true ||
            allocated.seed !== 71
        ) {
            throw new Error("import.meta receiver/url lost across allocation");
        }
        "#,
    )
    .expect("write module");

    run_module(&entry);
}

#[test]
fn error_builders_keep_prototypes_fields_and_iterator_values_rooted() {
    assert_interpreter_and_template(
        r#"
        function churn(seed) {
            let tail = null;
            for (let i = 0; i < 28; i++) {
                tail = { seed, i, text: "error-" + seed + "-" + i, tail };
            }
            return tail;
        }

        function dataDescriptor(object, key, expected) {
            const descriptor = Object.getOwnPropertyDescriptor(object, key);
            return descriptor.value === expected &&
                descriptor.writable === true &&
                descriptor.enumerable === false &&
                descriptor.configurable === true;
        }

        const errorEvents = [];
        const errorCause = { marker: "error-cause" };
        // Array is object-like but not an ordinary JsObject handle. This
        // exercises construct dispatch's full prototype Value path.
        const errorPrototype = [];
        const errorPrototypeSecondRead = { marker: "error-prototype-read-twice" };
        let errorPrototypeReads = 0;
        function ErrorNewTarget() {}
        const errorNewTarget = new Proxy(ErrorNewTarget, {
            get(target, key, receiver) {
                if (key === "prototype") {
                    errorEvents.push("prototype");
                    if (++errorPrototypeReads !== 1) {
                        throw errorPrototypeSecondRead;
                    }
                    churn(1);
                    return errorPrototype;
                }
                return Reflect.get(target, key, receiver);
            }
        });
        const errorMessage = {
            toString() {
                errorEvents.push("message");
                churn(2);
                return "rooted error";
            }
        };
        const errorOptions = {
            get cause() {
                errorEvents.push("cause");
                churn(3);
                return errorCause;
            }
        };
        const error = Reflect.construct(
            Error,
            [errorMessage, errorOptions],
            errorNewTarget
        );
        churn(4);

        const explicitObjectEvents = [];
        const explicitObjectSecondRead = {
            marker: "explicit-object-prototype-read-twice"
        };
        let explicitObjectReads = 0;
        function ExplicitObjectNewTarget() {}
        const explicitObjectNewTarget = new Proxy(ExplicitObjectNewTarget, {
            get(target, key, receiver) {
                if (key === "prototype") {
                    explicitObjectEvents.push("prototype");
                    if (++explicitObjectReads !== 1) {
                        throw explicitObjectSecondRead;
                    }
                    churn(5);
                    return Object.prototype;
                }
                return Reflect.get(target, key, receiver);
            }
        });
        const explicitObjectError = Reflect.construct(
            Error,
            ["explicit object prototype"],
            explicitObjectNewTarget
        );

        const errorFallbackEvents = [];
        const errorFallbackSecondRead = {
            marker: "error-fallback-prototype-read-twice"
        };
        let errorFallbackReads = 0;
        function ErrorFallbackNewTarget() {}
        const errorFallbackNewTarget = new Proxy(ErrorFallbackNewTarget, {
            get(target, key, receiver) {
                if (key === "prototype") {
                    errorFallbackEvents.push("prototype");
                    if (++errorFallbackReads !== 1) {
                        throw errorFallbackSecondRead;
                    }
                    churn(6);
                    return 17;
                }
                return Reflect.get(target, key, receiver);
            }
        });
        const fallbackError = Reflect.construct(
            Error,
            ["fallback error"],
            errorFallbackNewTarget
        );

        const first = { id: 1 };
        const second = { id: 2 };
        const aggregateCause = { marker: "aggregate-cause" };
        const aggregatePrototype = Object.create(AggregateError.prototype);
        const aggregateEvents = [];
        const aggregatePrototypeSecondRead = {
            marker: "aggregate-prototype-read-twice"
        };
        let aggregatePrototypeReads = 0;
        function AggregateNewTarget() {}
        const aggregateNewTarget = new Proxy(AggregateNewTarget, {
            get(target, key, receiver) {
                if (key === "prototype") {
                    aggregateEvents.push("prototype");
                    if (++aggregatePrototypeReads !== 1) {
                        throw aggregatePrototypeSecondRead;
                    }
                    churn(10);
                    return aggregatePrototype;
                }
                return Reflect.get(target, key, receiver);
            }
        });
        const aggregateMessage = {
            toString() {
                aggregateEvents.push("message");
                churn(11);
                return "rooted aggregate";
            }
        };
        const aggregateOptions = {
            get cause() {
                aggregateEvents.push("cause");
                churn(12);
                return aggregateCause;
            }
        };
        const input = [first, second];
        input[Symbol.iterator] = function () {
            aggregateEvents.push("iterator");
            let index = 0;
            return {
                next() {
                    aggregateEvents.push("next" + index);
                    churn(20 + index);
                    if (index < 2) return { value: input[index++], done: false };
                    return { value: undefined, done: true };
                }
            };
        };
        const aggregate = Reflect.construct(
            AggregateError,
            [input, aggregateMessage, aggregateOptions],
            aggregateNewTarget
        );
        churn(30);

        const aggregateFallbackEvents = [];
        const aggregateFallbackSecondRead = {
            marker: "aggregate-fallback-prototype-read-twice"
        };
        let aggregateFallbackReads = 0;
        function AggregateFallbackNewTarget() {}
        const aggregateFallbackNewTarget = new Proxy(
            AggregateFallbackNewTarget,
            {
                get(target, key, receiver) {
                    if (key === "prototype") {
                        aggregateFallbackEvents.push("prototype");
                        if (++aggregateFallbackReads !== 1) {
                            throw aggregateFallbackSecondRead;
                        }
                        churn(31);
                        return null;
                    }
                    return Reflect.get(target, key, receiver);
                }
            }
        );
        const fallbackAggregate = Reflect.construct(
            AggregateError,
            [[], "fallback aggregate"],
            aggregateFallbackNewTarget
        );

        const errorCallReceiver = { untouched: "error receiver" };
        const errorCallPrototype = Object.getPrototypeOf(errorCallReceiver);
        const calledError = Error.call(
            errorCallReceiver,
            "called error",
            { cause: errorCause }
        );
        const aggregateCallReceiver = { untouched: "aggregate receiver" };
        const aggregateCallPrototype =
            Object.getPrototypeOf(aggregateCallReceiver);
        const calledAggregate = AggregateError.call(
            aggregateCallReceiver,
            [first, second],
            "called aggregate",
            { cause: aggregateCause }
        );
        const proxyOptionsEvents = [];
        const proxyOptionsCause = { marker: "proxy-options-cause" };
        const proxyOptions = new Proxy(
            { cause: proxyOptionsCause },
            {
                has(target, key) {
                    proxyOptionsEvents.push("has:" + key);
                    churn(32);
                    return Reflect.has(target, key);
                },
                get(target, key, receiver) {
                    proxyOptionsEvents.push("get:" + key);
                    churn(33);
                    return Reflect.get(target, key, receiver);
                }
            }
        );
        const proxyOptionsError = new Error("proxy options", proxyOptions);
        const directError = new Error("direct error");
        const directAggregate = new AggregateError(
            [second],
            "direct aggregate"
        );
        churn(34);

        Object.getPrototypeOf(error) === errorPrototype &&
            Error.isError(error) === true &&
            error.message === "rooted error" &&
            error.cause === errorCause &&
            Reflect.ownKeys(error).join(",") === "message,cause" &&
            dataDescriptor(error, "message", "rooted error") &&
            dataDescriptor(error, "cause", errorCause) &&
            errorEvents.join(",") === "prototype,message,cause" &&
            Object.getPrototypeOf(explicitObjectError) === Object.prototype &&
            Error.isError(explicitObjectError) === true &&
            explicitObjectError.message === "explicit object prototype" &&
            explicitObjectEvents.join(",") === "prototype" &&
            Object.getPrototypeOf(fallbackError) === Error.prototype &&
            fallbackError.message === "fallback error" &&
            errorFallbackEvents.join(",") === "prototype" &&
            Object.getPrototypeOf(aggregate) === aggregatePrototype &&
            Error.isError(aggregate) === true &&
            aggregate.message === "rooted aggregate" &&
            aggregate.cause === aggregateCause &&
            Array.isArray(aggregate.errors) &&
            aggregate.errors.length === 2 &&
            aggregate.errors[0] === first &&
            aggregate.errors[1] === second &&
            Reflect.ownKeys(aggregate).join(",") === "message,cause,errors" &&
            dataDescriptor(aggregate, "message", "rooted aggregate") &&
            dataDescriptor(aggregate, "cause", aggregateCause) &&
            dataDescriptor(aggregate, "errors", aggregate.errors) &&
            aggregateEvents.join(",") ===
                "prototype,message,cause,iterator,next0,next1,next2" &&
            Object.getPrototypeOf(fallbackAggregate) ===
                AggregateError.prototype &&
            fallbackAggregate.message === "fallback aggregate" &&
            fallbackAggregate.errors.length === 0 &&
            aggregateFallbackEvents.join(",") === "prototype" &&
            calledError !== errorCallReceiver &&
            Error.isError(calledError) === true &&
            Object.getPrototypeOf(calledError) === Error.prototype &&
            calledError.message === "called error" &&
            calledError.cause === errorCause &&
            Object.getPrototypeOf(errorCallReceiver) === errorCallPrototype &&
            Reflect.ownKeys(errorCallReceiver).join(",") === "untouched" &&
            errorCallReceiver.untouched === "error receiver" &&
            calledAggregate !== aggregateCallReceiver &&
            Error.isError(calledAggregate) === true &&
            Object.getPrototypeOf(calledAggregate) === AggregateError.prototype &&
            calledAggregate.message === "called aggregate" &&
            calledAggregate.cause === aggregateCause &&
            calledAggregate.errors.length === 2 &&
            calledAggregate.errors[0] === first &&
            calledAggregate.errors[1] === second &&
            Object.getPrototypeOf(aggregateCallReceiver) ===
                aggregateCallPrototype &&
            Reflect.ownKeys(aggregateCallReceiver).join(",") === "untouched" &&
            aggregateCallReceiver.untouched === "aggregate receiver" &&
            proxyOptionsError.cause === proxyOptionsCause &&
            proxyOptionsEvents.join(",") === "has:cause,get:cause" &&
            Object.getPrototypeOf(directError) === Error.prototype &&
            directError.message === "direct error" &&
            Object.getPrototypeOf(directAggregate) ===
                AggregateError.prototype &&
            directAggregate.message === "direct aggregate" &&
            directAggregate.errors.length === 1 &&
            directAggregate.errors[0] === second;
        "#,
        "<gc-module-error-builders-success>",
        "true",
    );
}

#[test]
fn direct_proxy_call_and_construct_keep_boundary_slots_rooted() {
    assert_interpreter_and_template(
        r#"
        function churn(seed) {
            let tail = null;
            for (let i = 0; i < 28; i++) {
                tail = { seed, i, text: "proxy-" + seed + "-" + i, tail };
            }
            return tail;
        }

        const events = [];
        const first = { id: 1, tail: churn(1) };
        const second = { id: 2, tail: churn(2) };
        const trapResult = { marker: "trap-result", tail: churn(3) };
        let observedThis;
        let observedTarget;
        let observedArgs;
        let observedNewTarget;
        let trapReads = 0;
        function Target() {
            throw { wrong: "proxy target must not execute" };
        }
        const handler = {
            get construct() {
                events.push("get");
                if (++trapReads !== 1) {
                    throw { wrong: "construct trap read twice" };
                }
                churn(4);
                return function (target, args, newTarget) {
                    events.push("call");
                    observedThis = this;
                    observedTarget = target;
                    observedArgs = args;
                    observedNewTarget = newTarget;
                    churn(5);
                    return trapResult;
                };
            }
        };
        const proxy = new Proxy(Target, handler);
        const value = new proxy(first, second);
        churn(6);

        const syncEvents = [];
        const syncResult = { marker: "sync-trap-result", tail: churn(7) };
        let syncObservedThis;
        let syncObservedTarget;
        let syncObservedArgs;
        let syncObservedNewTarget;
        let syncTrapReads = 0;
        function SyncTarget() {
            throw { wrong: "sync proxy target must not execute" };
        }
        const syncHandler = {
            get construct() {
                syncEvents.push("get");
                if (++syncTrapReads !== 1) {
                    throw { wrong: "sync construct trap read twice" };
                }
                churn(8);
                return function (target, args, newTarget) {
                    syncEvents.push("call");
                    syncObservedThis = this;
                    syncObservedTarget = target;
                    syncObservedArgs = args;
                    syncObservedNewTarget = newTarget;
                    churn(9);
                    return syncResult;
                };
            }
        };
        const syncProxy = new Proxy(SyncTarget, syncHandler);
        const syncValue = Reflect.construct(
            syncProxy,
            [first, second],
            syncProxy
        );
        churn(10);

        const revokedEvents = [];
        const revokedResult = { marker: "revoked-trap-result", tail: churn(11) };
        let revokedObservedTarget;
        let revokedObservedNewTarget;
        let revokeDirect;
        function RevokedTarget() {
            throw { wrong: "revoked proxy target must not execute" };
        }
        const revokedHandler = {
            get construct() {
                revokedEvents.push("get");
                const trap = function (target, args, newTarget) {
                    revokedEvents.push("call");
                    revokedObservedTarget = target;
                    revokedObservedNewTarget = newTarget;
                    churn(12);
                    return revokedResult;
                };
                revokeDirect();
                churn(13);
                return trap;
            }
        };
        const directRevocable = Proxy.revocable(
            RevokedTarget,
            revokedHandler
        );
        const revokedProxy = directRevocable.proxy;
        revokeDirect = directRevocable.revoke;
        const revokedValue = new revokedProxy(first);

        const fallbackEvents = [];
        const fallbackPrototype = { marker: "fallback-prototype", tail: churn(14) };
        let fallbackTargetRan = false;
        function FallbackTarget(arg) {
            fallbackEvents.push("target");
            fallbackTargetRan = true;
            this.arg = arg;
            churn(15);
        }
        function FallbackNewTarget() {}
        FallbackNewTarget.prototype = fallbackPrototype;
        let revokeFallback;
        const fallbackHandler = {
            get construct() {
                fallbackEvents.push("get");
                revokeFallback();
                churn(16);
                return undefined;
            }
        };
        const fallbackRevocable = Proxy.revocable(
            FallbackTarget,
            fallbackHandler
        );
        revokeFallback = fallbackRevocable.revoke;
        const fallbackValue = Reflect.construct(
            fallbackRevocable.proxy,
            [second],
            FallbackNewTarget
        );
        churn(17);

        const callEvents = [];
        let revokeCall;
        function CallTarget(arg) {
            callEvents.push("target");
            churn(18);
            return arg;
        }
        const callHandler = {
            get apply() {
                callEvents.push("get");
                revokeCall();
                churn(19);
                return undefined;
            }
        };
        const callRevocable = Proxy.revocable(CallTarget, callHandler);
        const callProxy = callRevocable.proxy;
        revokeCall = callRevocable.revoke;
        const callValue = callProxy(first);
        churn(20);

        value === trapResult &&
            trapReads === 1 &&
            events.join(",") === "get,call" &&
            observedThis === handler &&
            observedTarget === Target &&
            observedNewTarget === proxy &&
            Array.isArray(observedArgs) &&
            observedArgs.length === 2 &&
            observedArgs[0] === first &&
            observedArgs[1] === second &&
            syncValue === syncResult &&
            syncTrapReads === 1 &&
            syncEvents.join(",") === "get,call" &&
            syncObservedThis === syncHandler &&
            syncObservedTarget === SyncTarget &&
            syncObservedNewTarget === syncProxy &&
            Array.isArray(syncObservedArgs) &&
            syncObservedArgs.length === 2 &&
            syncObservedArgs[0] === first &&
            syncObservedArgs[1] === second &&
            revokedValue === revokedResult &&
            revokedEvents.join(",") === "get,call" &&
            revokedObservedTarget === RevokedTarget &&
            revokedObservedNewTarget === revokedProxy &&
            fallbackTargetRan === true &&
            fallbackEvents.join(",") === "get,target" &&
            Object.getPrototypeOf(fallbackValue) === fallbackPrototype &&
            fallbackValue.arg === second &&
            callValue === first &&
            callEvents.join(",") === "get,target";
        "#,
        "<gc-direct-proxy-construct-trap>",
        "true",
    );
}

#[test]
fn direct_derived_super_construct_keeps_one_rooted_boundary() {
    assert_interpreter_and_template(
        r#"
        function churn(seed) {
            let tail = null;
            for (let i = 0; i < 28; i++) {
                tail = { seed, i, text: "direct-" + seed + "-" + i, tail };
            }
            return tail;
        }

        const events = [];
        const cause = churn(1);
        const message = {
            toString() {
                events.push("message");
                churn(2);
                return "bound-message";
            }
        };
        const options = {
            get cause() {
                events.push("cause");
                churn(3);
                return cause;
            }
        };
        const ignoredMessage = {
            toString() {
                throw { wrong: "message" };
            }
        };
        const ignoredOptions = {
            get cause() {
                throw { wrong: "cause" };
            }
        };

        const BoundError = Error.bind(null, message, options);
        BoundError.prototype = Object.create(Error.prototype);
        let superResult;
        let thisResult;
        let seenNewTarget;
        class DerivedError extends BoundError {
            constructor(...args) {
                const result = super(...args);
                superResult = result;
                thisResult = this;
                seenNewTarget = new.target;
                events.push("body");
            }
        }

        // Ordinary `new DerivedError` takes the register-window fast path. It
        // must defer receiver creation to `super` without materializing args.
        const warmup = new DerivedError(ignoredMessage, ignoredOptions);
        const warmupOk =
            warmup === superResult &&
            warmup === thisResult &&
            seenNewTarget === DerivedError &&
            Object.getPrototypeOf(warmup) === DerivedError.prototype &&
            warmup.message === "bound-message" &&
            warmup.cause === cause &&
            Error.isError(warmup) === true;
        events.length = 0;
        superResult = undefined;
        thisResult = undefined;
        seenNewTarget = undefined;

        const customPrototype = { tag: churn(4) };
        const secondPrototypeRead = {
            marker: "direct-prototype-read-twice"
        };
        let prototypeReads = 0;
        let ProxiedDerived;
        ProxiedDerived = new Proxy(DerivedError, {
            get(target, key, receiver) {
                if (key === "prototype") {
                    events.push("prototype");
                    if (++prototypeReads !== 1) {
                        throw secondPrototypeRead;
                    }
                    churn(5);
                    return customPrototype;
                }
                return Reflect.get(target, key, receiver);
            }
        });

        const value = new ProxiedDerived(ignoredMessage, ignoredOptions);
        churn(6);

        warmupOk &&
            prototypeReads === 1 &&
            events.join(",") === "prototype,message,cause,body" &&
            seenNewTarget === ProxiedDerived &&
            value === superResult &&
            value === thisResult &&
            Object.getPrototypeOf(value) === customPrototype &&
            value.message === "bound-message" &&
            value.cause === cause &&
            Error.isError(value) === true;
        "#,
        "<gc-direct-derived-super-construct>",
        "true",
    );
}

#[test]
fn error_constructor_abrupt_completions_stop_in_spec_order() {
    assert_interpreter_and_template(
        r#"
        function churn(seed) {
            let tail = null;
            for (let i = 0; i < 24; i++) tail = { seed, i, tail };
            return tail;
        }

        const errorPrototypeEvents = [];
        const errorPrototypeSentinel = { marker: "error-prototype-abrupt" };
        function ThrowingErrorNewTarget() {}
        const throwingErrorNewTarget = new Proxy(ThrowingErrorNewTarget, {
            get(target, key, receiver) {
                if (key === "prototype") {
                    errorPrototypeEvents.push("prototype");
                    churn(35);
                    throw errorPrototypeSentinel;
                }
                return Reflect.get(target, key, receiver);
            }
        });
        let errorPrototypeCaught = false;
        try {
            Reflect.construct(
                Error,
                [{
                    toString() {
                        errorPrototypeEvents.push("message");
                        return "skipped";
                    }
                }],
                throwingErrorNewTarget
            );
        } catch (error) {
            errorPrototypeCaught = error === errorPrototypeSentinel;
        }

        const aggregatePrototypeEvents = [];
        const aggregatePrototypeSentinel = {
            marker: "aggregate-prototype-abrupt"
        };
        function ThrowingAggregateNewTarget() {}
        const throwingAggregateNewTarget = new Proxy(
            ThrowingAggregateNewTarget,
            {
                get(target, key, receiver) {
                    if (key === "prototype") {
                        aggregatePrototypeEvents.push("prototype");
                        churn(36);
                        throw aggregatePrototypeSentinel;
                    }
                    return Reflect.get(target, key, receiver);
                }
            }
        );
        const skippedAggregateInput = {
            [Symbol.iterator]() {
                aggregatePrototypeEvents.push("iterator");
                return { next() { return { done: true }; } };
            }
        };
        let aggregatePrototypeCaught = false;
        try {
            Reflect.construct(
                AggregateError,
                [skippedAggregateInput, {
                    toString() {
                        aggregatePrototypeEvents.push("message");
                        return "skipped";
                    }
                }],
                throwingAggregateNewTarget
            );
        } catch (error) {
            aggregatePrototypeCaught = error === aggregatePrototypeSentinel;
        }
        const reusedError = new Error("reuse-error");
        const reusedAggregate = new AggregateError([], "reuse-aggregate");
        churn(37);

        const messageEvents = [];
        const messageSentinel = { marker: "message-abrupt" };
        function MessageNewTarget() {}
        const messageNewTarget = new Proxy(MessageNewTarget, {
            get(target, key, receiver) {
                if (key === "prototype") {
                    messageEvents.push("prototype");
                    churn(40);
                    return AggregateError.prototype;
                }
                return Reflect.get(target, key, receiver);
            }
        });
        const throwingMessage = {
            toString() {
                messageEvents.push("message");
                churn(41);
                throw messageSentinel;
            }
        };
        const skippedOptions = {
            get cause() {
                messageEvents.push("cause");
                return null;
            }
        };
        const skippedErrors = {
            [Symbol.iterator]() {
                messageEvents.push("iterator");
                return { next() { return { done: true }; } };
            }
        };
        let messageCaught = false;
        try {
            Reflect.construct(
                AggregateError,
                [skippedErrors, throwingMessage, skippedOptions],
                messageNewTarget
            );
        } catch (error) {
            messageCaught = error === messageSentinel;
        }

        const iteratorEvents = [];
        let iteratorReturnCount = 0;
        const iteratorSentinel = { marker: "iterator-abrupt" };
        function IteratorNewTarget() {}
        const iteratorNewTarget = new Proxy(IteratorNewTarget, {
            get(target, key, receiver) {
                if (key === "prototype") {
                    iteratorEvents.push("prototype");
                    churn(50);
                    return AggregateError.prototype;
                }
                return Reflect.get(target, key, receiver);
            }
        });
        const iteratorMessage = {
            toString() {
                iteratorEvents.push("message");
                churn(51);
                return "iterator outer";
            }
        };
        const iteratorOptions = {
            get cause() {
                iteratorEvents.push("cause");
                churn(52);
                return { marker: "cause" };
            }
        };
        const throwingErrors = {
            [Symbol.iterator]() {
                iteratorEvents.push("iterator");
                return {
                    next() {
                        iteratorEvents.push("next");
                        churn(53);
                        throw iteratorSentinel;
                    },
                    return() {
                        iteratorReturnCount++;
                        iteratorEvents.push("return");
                        throw { wrong: "iterator return must not run" };
                    }
                };
            }
        };
        let iteratorCaught = false;
        try {
            Reflect.construct(
                AggregateError,
                [throwingErrors, iteratorMessage, iteratorOptions],
                iteratorNewTarget
            );
        } catch (error) {
            iteratorCaught = error === iteratorSentinel;
        }

        errorPrototypeCaught &&
            aggregatePrototypeCaught &&
            messageCaught &&
            iteratorCaught &&
            iteratorReturnCount === 0 &&
            reusedError.message === "reuse-error" &&
            reusedAggregate.message === "reuse-aggregate" &&
            reusedAggregate.errors.length === 0 &&
            errorPrototypeEvents.join(",") === "prototype" &&
            aggregatePrototypeEvents.join(",") === "prototype" &&
            messageEvents.join(",") === "prototype,message" &&
            iteratorEvents.join(",") ===
                "prototype,message,cause,iterator,next";
        "#,
        "<gc-module-error-builders-abrupt>",
        "true",
    );
}
