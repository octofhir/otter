// Worker wrapper - provides Web Worker API

(function() {
    'use strict';

    const workers = new Map();

    // Worker class
    class Worker {
        constructor(scriptURL, options) {
            // For inline workers, use URL.createObjectURL with Blob
            // For file URLs, pass the path directly
            this._script = scriptURL;
            this._id = workerCreate(scriptURL);
            this._terminated = false;

            // Event handlers
            this.onmessage = null;
            this.onerror = null;
            this.onmessageerror = null;

            // Register for event polling
            workers.set(this._id, this);
        }

        postMessage(message, transfer) {
            if (this._terminated) {
                throw new Error('Worker has been terminated');
            }
            workerPostMessage(this._id, message);
        }

        terminate() {
            if (!this._terminated) {
                this._terminated = true;
                workerTerminate(this._id);
                workers.delete(this._id);
            }
        }

        // Internal: dispatch event
        _dispatchEvent(type, data) {
            const event = { type, target: this, ...data };

            switch (type) {
                case 'message':
                    event.data = data.data;
                    if (this.onmessage) this.onmessage(event);
                    break;
                case 'error':
                    event.message = data.message;
                    if (this.onerror) this.onerror(event);
                    break;
                case 'exit':
                case 'terminated':
                    workers.delete(this._id);
                    break;
            }
        }
    }

    // Poll for Worker events (called from event loop)
    globalThis.__otter_worker_poll = function() {
        const events = workerPollEvents();
        for (const event of events) {
            const worker = workers.get(event.id);
            if (worker) {
                worker._dispatchEvent(event.type, event);
            }
        }
        return events.length;
    };

    // Export
    globalThis.Worker = Worker;
})();
