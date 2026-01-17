// process IPC module - sets up process.send and message polling

(function() {
    'use strict';

    if (!globalThis.process) return;

    // Mark process as connected
    process.connected = true;

    // Override send to use IPC
    process.send = function(message, _handle, _options, callback) {
        if (typeof callback !== 'function' && typeof _handle === 'function') {
            callback = _handle;
        }
        if (typeof callback !== 'function' && typeof _options === 'function') {
            callback = _options;
        }

        return __otter_process_ipc_send(message)
            .then((result) => {
                if (callback) callback(null);
                return result;
            })
            .catch((err) => {
                if (callback) callback(err);
                return false;
            });
    };

    // Override disconnect
    process.disconnect = function() {
        __otter_process_ipc_disconnect();
        process.connected = false;
        process.emit('disconnect');
    };

    // Set up message polling
    let pollInterval = null;
    const pollMessages = async () => {
        if (!process.connected) {
            if (pollInterval) clearInterval(pollInterval);
            return;
        }

        try {
            const msg = await __otter_process_ipc_recv();
            if (msg !== null) {
                process.emit('message', msg);
            }
        } catch (e) {
            console.error('IPC poll error:', e);
        }
    };

    // Start polling when there are message listeners
    const origOn = process.on.bind(process);
    process.on = function(event, handler) {
        if (event === 'message' && !pollInterval) {
            pollInterval = setInterval(pollMessages, 10);
        }
        return origOn(event, handler);
    };
})();
