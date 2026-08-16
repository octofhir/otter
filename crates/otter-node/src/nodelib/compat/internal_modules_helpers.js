'use strict';
module.exports = {
  stripBOM(content) { return content.charCodeAt(0) === 0xFEFF ? content.slice(1) : content; },
};
