/* otter-test:
name = "ts: declare statements erase to nothing"
[expect]
exit_code = 0
*/
declare function externalFn(x: number): string;
declare const externalConst: number;
declare class ExternalClass { method(): void; }
undefined;
