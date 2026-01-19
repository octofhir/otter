import express from 'express';

const app = express();
const PORT = 8080;

// JSON middleware
app.use(express.json());

// Root route
app.get('/', (req, res) => {
    res.send('<h1>Hello from Express on Otter!</h1><p>The server is running correctly.</p>');
});

// JSON endpoint
app.get('/api/status', (req, res) => {
    res.json({
        status: 'ok',
        runtime: 'otter',
        timestamp: new Date().toISOString()
    });
});

// Start server
app.listen(PORT, () => {
    console.log(`Express server running at http://localhost:${PORT}`);
    console.log(`Try opening http://localhost:${PORT} in your browser`);
    console.log(`Or http://localhost:${PORT}/api/status for JSON`);
});
