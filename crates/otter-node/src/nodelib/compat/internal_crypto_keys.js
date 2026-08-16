'use strict';
// Brand classes for instanceof checks in deep-equal; real key objects come
// from the crypto module when it grows them.
class KeyObject {}
class CryptoKey {}
module.exports = { KeyObject, CryptoKey };
