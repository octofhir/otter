// TypeScript generics example

// Generic class
class Container<T> {
    private value: T;

    constructor(value: T) {
        this.value = value;
    }

    getValue(): T {
        return this.value;
    }

    setValue(value: T): void {
        this.value = value;
    }
}

const numberContainer = new Container<number>(42);
console.log("Container value:", numberContainer.getValue());

const stringContainer = new Container<string>("Hello TypeScript");
console.log("Container value:", stringContainer.getValue());

// Generic function with constraints
interface HasLength {
    length: number;
}

function logLength<T extends HasLength>(item: T): number {
    console.log("Length:", item.length);
    return item.length;
}

logLength("hello");
logLength([1, 2, 3, 4, 5]);
logLength({ length: 10, name: "custom" });

// Multiple type parameters
function pair<K, V>(key: K, value: V): [K, V] {
    return [key, value];
}

const kvPair = pair("name", "Alice");
console.log("Pair:", kvPair);

// Generic type alias
type Nullable<T> = T | null;

const maybeNumber: Nullable<number> = 42;
const maybeString: Nullable<string> = null;

console.log("Maybe number:", maybeNumber);
console.log("Maybe string:", maybeString);
