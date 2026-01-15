// TypeScript classes and OOP example

// Abstract class
abstract class Animal {
    constructor(public name: string) {}

    abstract makeSound(): string;

    move(distance: number): void {
        console.log(`${this.name} moved ${distance} meters.`);
    }
}

// Class extending abstract class
class Dog extends Animal {
    constructor(name: string, public breed: string) {
        super(name);
    }

    makeSound(): string {
        return "Woof!";
    }

    fetch(): void {
        console.log(`${this.name} is fetching the ball!`);
    }
}

class Cat extends Animal {
    constructor(name: string, public indoor: boolean) {
        super(name);
    }

    makeSound(): string {
        return "Meow!";
    }

    scratch(): void {
        console.log(`${this.name} is scratching the furniture!`);
    }
}

// Interface for dependency injection
interface Logger {
    log(message: string): void;
}

class ConsoleLogger implements Logger {
    log(message: string): void {
        console.log(`[LOG] ${message}`);
    }
}

// Class with private/readonly members
class BankAccount {
    private balance: number;
    readonly accountNumber: string;

    constructor(accountNumber: string, initialBalance: number) {
        this.accountNumber = accountNumber;
        this.balance = initialBalance;
    }

    deposit(amount: number): void {
        if (amount > 0) {
            this.balance += amount;
            console.log(`Deposited $${amount}. New balance: $${this.balance}`);
        }
    }

    withdraw(amount: number): boolean {
        if (amount > 0 && amount <= this.balance) {
            this.balance -= amount;
            console.log(`Withdrew $${amount}. New balance: $${this.balance}`);
            return true;
        }
        console.log("Insufficient funds!");
        return false;
    }

    getBalance(): number {
        return this.balance;
    }
}

// Static members
class MathUtils {
    static readonly PI = 3.14159;

    static square(n: number): number {
        return n * n;
    }

    static cube(n: number): number {
        return n * n * n;
    }
}

// Demo
console.log("=== TypeScript Classes Demo ===\n");

const dog = new Dog("Buddy", "Golden Retriever");
console.log(`${dog.name} says: ${dog.makeSound()}`);
dog.move(10);
dog.fetch();

console.log("");

const cat = new Cat("Whiskers", true);
console.log(`${cat.name} says: ${cat.makeSound()}`);
cat.scratch();

console.log("");

const logger = new ConsoleLogger();
logger.log("Testing the logger");

console.log("");

const account = new BankAccount("ACC-001", 1000);
account.deposit(500);
account.withdraw(200);
console.log(`Final balance: $${account.getBalance()}`);

console.log("");

console.log("PI:", MathUtils.PI);
console.log("5 squared:", MathUtils.square(5));
console.log("3 cubed:", MathUtils.cube(3));
