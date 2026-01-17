import process from "node:process";

const pid: number = process.pid;
const rss: number = process.memoryUsage().rss;
const args: string[] = process.argv;

console.log(process.cwd(), pid, rss, args.length);
