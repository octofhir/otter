// Test262 standard test assertions

var $ERROR = function(message) {
    throw new Error(message);
};

function Test262Error(message) {
    this.message = message || "";
}
Test262Error.prototype.toString = function() {
    return "Test262Error: " + this.message;
};

var $DONE = function(error) {
    if (error) {
        if (typeof error === "object" && error !== null && "message" in error) {
            __test262_done(false, error.message);
        } else {
            __test262_done(false, String(error));
        }
    } else {
        __test262_done(true, "");
    }
};
