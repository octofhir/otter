"use strict";
function same(n, acc) { return n === 0 ? acc : same(n - 1, acc + 1); }
function shrink(n, a, b, c) { return n === 0 ? a : shrinkTo(n - 1, a + 1); }
function shrinkTo(n, a) { return shrink(n, a, 0, 0); }
function even(n) { return n === 0 ? true : odd(n - 1); }
function odd(n) { return n === 0 ? false : even(n - 1); }
const arrow = (n, acc) => n === 0 ? acc : arrow(n - 1, acc + 2);
function growChain(n) { return n === 0 ? 'done' : growChain2(n - 1, n, n, n, n, n, n); }
function growChain2(n, a, b, c, d, e, f) { return growChain(n); }
const cases = { same: () => same(1e6, 0), shrink: () => shrink(1e6, 0, 0, 0), even: () => even(1e6), arrow: () => arrow(1e6, 0), grow: () => growChain(1e6) };
const t0 = Date.now();
try { console.log(process.argv[2], cases[process.argv[2]](), Date.now() - t0, 'ms'); } catch (e) { console.log(process.argv[2], e.name, Date.now() - t0, 'ms'); }
