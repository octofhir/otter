// TypeScript async/await example

interface ApiResponse<T> {
    data: T;
    status: number;
    timestamp: Date;
}

// Simulated async function
async function fetchData<T>(data: T, delay: number): Promise<ApiResponse<T>> {
    return new Promise((resolve) => {
        setTimeout(() => {
            resolve({
                data,
                status: 200,
                timestamp: new Date()
            });
        }, delay);
    });
}

// Async function with typed response
async function getUser(): Promise<{ name: string; id: number }> {
    const response = await fetchData({ name: "Bob", id: 123 }, 100);
    return response.data;
}

// Multiple async operations
async function fetchAll(): Promise<void> {
    console.log("Starting async operations...");

    const [user, posts, settings] = await Promise.all([
        fetchData({ name: "Alice", id: 1 }, 50),
        fetchData(["post1", "post2", "post3"], 100),
        fetchData({ theme: "dark", notifications: true }, 75)
    ]);

    console.log("User:", user.data);
    console.log("Posts:", posts.data);
    console.log("Settings:", settings.data);
}

// Error handling with types
async function safeFetch<T>(data: T): Promise<T | Error> {
    try {
        const response = await fetchData(data, 50);
        return response.data;
    } catch (error) {
        return error as Error;
    }
}

// Run the examples
async function main(): Promise<void> {
    console.log("=== Async TypeScript Examples ===\n");

    const user = await getUser();
    console.log("Got user:", user);

    await fetchAll();

    const result = await safeFetch({ message: "Hello async!" });
    console.log("Safe fetch result:", result);

    console.log("\n=== Done ===");
}

main();
