// Quick k6 test for Otter.serve()
import http from 'k6/http';
import { check, sleep } from 'k6';

export const options = {
    vus: 10,          // 10 virtual users
    duration: '10s',  // 10 seconds
};

export default function () {
    const res = http.get('http://localhost:3001/');
    check(res, {
        'status is 200': (r) => r.status === 200,
        'body is correct': (r) => r.body === 'Hello, World!',
    });
}
