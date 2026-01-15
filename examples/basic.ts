// Basic TypeScript example demonstrating type annotations

interface User {
    name: string;
    age: number;
    email?: string;
}

function greet(user: User): string {
    return `Hello, ${user.name}! You are ${user.age} years old.`;
}

const user: User = {
    name: "Alice",
    age: 30,
    email: "alice@example.com"
};

console.log(greet(user));

// Generic function
function identity<T>(value: T): T {
    return value;
}

console.log("Number:", identity(42));
console.log("String:", identity("hello"));

// Array methods with types
const numbers: number[] = [1, 2, 3, 4, 5];
const doubled = numbers.map((n: number): number => n * 2);
console.log("Doubled:", doubled);

// Union types
type Result = "success" | "error" | "pending";
const currentStatus: Result = "success";
console.log("Status:", currentStatus);
