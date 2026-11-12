// FindViaPredicate uses Get at every visited index and never HasProperty.
(() => {
    function check(condition, message) {
        if (!condition) throw new Error(message);
    }
    function churn() {
        let held = null;
        for (let i = 0; i < 12; i++) held = {i, held};
        return held;
    }
    const methods = ['find', 'findIndex', 'findLast', 'findLastIndex'];
    for (const name of methods) {
        const reverse = name === 'findLast' || name === 'findLastIndex';
        const returnsIndex = name === 'findIndex' || name === 'findLastIndex';
        const indices = reverse ? [2, 1, 0] : [0, 1, 2];
        const events = [];
        const receiver = new Proxy({length: 3}, {
            get(target, key) {
                events.push('get:' + key);
                if (key === 'length') return target.length;
                churn();
                return {index: Number(key)};
            },
            has() { throw new Error(name + ' invoked has'); }
        });
        const thisArg = {name};
        const result = Array.prototype[name].call(receiver, function (value, index, object) {
            churn();
            check(this === thisArg && object === receiver, name + ' callback identity');
            check(value.index === index, name + ' lost getter result');
            events.push('predicate:' + index);
            return false;
        }, thisArg);
        const expected = ['get:length'];
        for (const index of indices) expected.push('get:' + index, 'predicate:' + index);
        check(events.join('|') === expected.join('|'), name + ' property order');
        check(result === (returnsIndex ? -1 : undefined), name + ' no-match result');

        // An early match returns the actual element, even if its callback collects.
        const marker = {name};
        const earlyEvents = [];
        const early = new Proxy({length: 3}, {
            get(target, key) {
                earlyEvents.push(key);
                return key === 'length' ? 3 : marker;
            },
            has() { throw new Error(name + ' early has'); }
        });
        const match = Array.prototype[name].call(early, value => {
            churn();
            return value === marker;
        });
        check(match === (returnsIndex ? indices[0] : marker), name + ' early result');
        check(earlyEvents.join('|') === 'length|' + indices[0], name + ' read after match');

        // A throwing Get stops immediately and preserves the thrown object's identity.
        let calls = 0;
        const abruptEvents = [];
        const abrupt = new Proxy({length: 3}, {
            get(target, key) {
                abruptEvents.push(key);
                if (key === 'length') return 3;
                if (key === String(indices[1])) {
                    churn();
                    throw marker;
                }
                return undefined;
            },
            has() { throw new Error(name + ' abrupt has'); }
        });
        let caught;
        try {
            Array.prototype[name].call(abrupt, () => { calls++; return false; });
        } catch (error) { caught = error; }
        check(caught === marker && calls === 1, name + ' Get exception');
        check(abruptEvents.join('|') === ['length', indices[0], indices[1]].join('|'),
            name + ' read after exception');

        // Arrays must also visit holes and read own/inherited accessors once.
        const arrayEvents = [];
        const array = new Array(3);
        const prototype = Object.create(Array.prototype);
        Object.defineProperty(prototype, '0', {
            get() { arrayEvents.push('inherited'); return 'prototype'; }
        });
        Object.setPrototypeOf(array, prototype);
        Object.defineProperty(array, '1', {
            get() { arrayEvents.push('own'); return 'own'; }
        });
        const values = [];
        Array.prototype[name].call(array, (value, index) => {
            values.push(index + ':' + value);
            return false;
        });
        check(values.join('|') === (reverse ? '2:undefined|1:own|0:prototype'
            : '0:prototype|1:own|2:undefined'), name + ' hole/accessor values');
        check(arrayEvents.join('|') === (reverse ? 'own|inherited' : 'inherited|own'),
            name + ' accessor count/order');
    }

    // Other callback methods still use HasProperty and skip absent elements.
    for (const name of ['forEach', 'map', 'filter', 'some', 'every', 'reduce', 'reduceRight']) {
        const events = [];
        let calls = 0;
        const receiver = new Proxy({length: 2}, {
            get(target, key) {
                events.push('get:' + key);
                if (key === 'length') return 2;
                throw new Error(name + ' read absent index');
            },
            has(target, key) { events.push('has:' + key); return false; }
        });
        Array.prototype[name].call(receiver, () => { calls++; }, 0);
        const indices = name === 'reduceRight' ? [1, 0] : [0, 1];
        check(calls === 0, name + ' visited absent index');
        check(events.join('|') === ['get:length', 'has:' + indices[0],
            'has:' + indices[1]].join('|'), name + ' lost HasProperty protocol');
    }
    return 'ok:find-property-protocol';
})()
