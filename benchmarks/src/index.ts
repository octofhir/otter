// Welcome to Otter!

interface Greeting {
  message: string;
  timestamp: Date;
}

function greet(name: string): Greeting {
  return {
    message: `Hello, ${name}! Welcome to Otter.`,
    timestamp: new Date(),
  };
}

const greeting = greet("World");
console.log(greeting.message);
console.log(`Started at: ${greeting.timestamp.toISOString()}`);
