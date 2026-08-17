'use strict';
// internal/url — URL plus the http-facing helpers the vendored client uses.

function isURL(value) {
  return typeof URL !== 'undefined' && value instanceof URL;
}

function pathToFileURL(path) {
  return new URL(`file://${String(path)}`);
}

// §https://nodejs.org/api/url.html#urlurltohttpoptionsurl
function urlToHttpOptions(url) {
  const { hostname, pathname, port, username, password, search } = url;
  const options = {
    __proto__: null,
    ...url, // In case the url object was extended by the user.
    protocol: url.protocol,
    hostname: typeof hostname === 'string' && hostname.startsWith('[') ?
      hostname.slice(1, -1) :
      hostname,
    hash: url.hash,
    search,
    pathname,
    path: `${pathname || ''}${search || ''}`,
    href: url.href,
  };
  if (port !== '') {
    options.port = Number(port);
  }
  if (username || password) {
    options.auth = `${decodeURIComponent(username)}:${decodeURIComponent(password)}`;
  }
  return options;
}

module.exports = {
  URL,
  URLSearchParams,
  isURL,
  pathToFileURL,
  urlToHttpOptions,
  domainToASCII: (domain) => String(domain).toLowerCase(),
  domainToUnicode: (domain) => String(domain),
};
