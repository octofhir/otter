// WebSocket wrapper - provides Web-standard WebSocket API

(function() {
    'use strict';

    const connections = new Map();

    // WebSocket class
    class WebSocket {
        static CONNECTING = 0;
        static OPEN = 1;
        static CLOSING = 2;
        static CLOSED = 3;

        constructor(url, protocols) {
            this._id = wsConnect(url);
            this._url = url;
            this._protocols = protocols || [];
            this._binaryType = 'blob';

            // Event handlers
            this.onopen = null;
            this.onmessage = null;
            this.onclose = null;
            this.onerror = null;

            // Register for event polling
            connections.set(this._id, this);
        }

        get url() {
            return this._url;
        }

        get readyState() {
            return wsReadyState(this._id);
        }

        get protocol() {
            return '';
        }

        get extensions() {
            return '';
        }

        get binaryType() {
            return this._binaryType;
        }

        set binaryType(value) {
            this._binaryType = value;
        }

        get bufferedAmount() {
            return 0;
        }

        send(data) {
            wsSend(this._id, data);
        }

        close(code, reason) {
            wsClose(this._id, code, reason);
        }

        // Internal: dispatch event
        _dispatchEvent(type, data) {
            const event = { type, target: this, ...data };

            switch (type) {
                case 'open':
                    if (this.onopen) this.onopen(event);
                    break;
                case 'message':
                    event.data = data.data;
                    if (this.onmessage) this.onmessage(event);
                    break;
                case 'close':
                    event.code = data.code;
                    event.reason = data.reason;
                    event.wasClean = data.code === 1000;
                    if (this.onclose) this.onclose(event);
                    connections.delete(this._id);
                    break;
                case 'error':
                    if (this.onerror) this.onerror(event);
                    break;
            }
        }
    }

    // Poll for WebSocket events (called from event loop)
    globalThis.__otter_ws_poll = function() {
        const events = wsPollEvents();
        for (const event of events) {
            const ws = connections.get(event.id);
            if (ws) {
                ws._dispatchEvent(event.type, event);
            }
        }
        return events.length;
    };

    // Export
    globalThis.WebSocket = WebSocket;
})();
