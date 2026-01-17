//! Native URL Web API implementation for Otter.
//!
//! Provides WHATWG URL Standard compliant URL and URLSearchParams classes.
//! Uses the `url` crate for parsing and manipulation.

use serde::{Deserialize, Serialize};
use url::Url;

/// Parsed URL components for JavaScript access.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UrlComponents {
    pub href: String,
    pub origin: String,
    pub protocol: String,
    pub username: String,
    pub password: String,
    pub host: String,
    pub hostname: String,
    pub port: String,
    pub pathname: String,
    pub search: String,
    pub hash: String,
}

impl UrlComponents {
    /// Parse a URL string, optionally with a base URL.
    pub fn parse(url_string: &str, base: Option<&str>) -> Result<Self, String> {
        let parsed = if let Some(base_str) = base {
            let base_url = Url::parse(base_str)
                .map_err(|e| format!("Invalid base URL: {}", e))?;
            base_url.join(url_string)
                .map_err(|e| format!("Invalid URL: {}", e))?
        } else {
            Url::parse(url_string)
                .map_err(|e| format!("Invalid URL: {}", e))?
        };

        Ok(Self::from_url(&parsed))
    }

    /// Create components from a parsed Url.
    fn from_url(url: &Url) -> Self {
        let origin = url.origin().ascii_serialization();

        // Protocol includes the trailing colon
        let protocol = format!("{}:", url.scheme());

        // Host includes port if non-default
        let host = url.host_str()
            .map(|h| {
                if let Some(port) = url.port() {
                    format!("{}:{}", h, port)
                } else {
                    h.to_string()
                }
            })
            .unwrap_or_default();

        // Hostname is just the host without port
        let hostname = url.host_str().unwrap_or("").to_string();

        // Port as string (empty if default)
        let port = url.port().map(|p| p.to_string()).unwrap_or_default();

        // Pathname (default to "/" for URLs with authority)
        let pathname = if url.cannot_be_a_base() {
            url.path().to_string()
        } else if url.path().is_empty() {
            "/".to_string()
        } else {
            url.path().to_string()
        };

        // Search includes leading "?" if present
        let search = url.query()
            .map(|q| format!("?{}", q))
            .unwrap_or_default();

        // Hash includes leading "#" if present
        let hash = url.fragment()
            .map(|f| format!("#{}", f))
            .unwrap_or_default();

        Self {
            href: url.to_string(),
            origin,
            protocol,
            username: url.username().to_string(),
            password: url.password().unwrap_or("").to_string(),
            host,
            hostname,
            port,
            pathname,
            search,
            hash,
        }
    }

    /// Set a URL component and return updated components.
    pub fn set_component(&self, name: &str, value: &str) -> Result<Self, String> {
        let mut url = Url::parse(&self.href)
            .map_err(|e| format!("Invalid URL: {}", e))?;

        match name {
            "href" => {
                return Self::parse(value, None);
            }
            "protocol" => {
                // Remove trailing colon if present
                let scheme = value.trim_end_matches(':');
                url.set_scheme(scheme)
                    .map_err(|_| "Invalid protocol")?;
            }
            "username" => {
                url.set_username(value)
                    .map_err(|_| "Cannot set username")?;
            }
            "password" => {
                url.set_password(if value.is_empty() { None } else { Some(value) })
                    .map_err(|_| "Cannot set password")?;
            }
            "host" => {
                // Host may include port
                if let Some(colon_idx) = value.rfind(':') {
                    let (host_part, port_part) = value.split_at(colon_idx);
                    url.set_host(Some(host_part))
                        .map_err(|e| format!("Invalid host: {}", e))?;
                    if let Ok(port) = port_part[1..].parse::<u16>() {
                        url.set_port(Some(port))
                            .map_err(|_| "Cannot set port")?;
                    }
                } else {
                    url.set_host(Some(value))
                        .map_err(|e| format!("Invalid host: {}", e))?;
                }
            }
            "hostname" => {
                url.set_host(Some(value))
                    .map_err(|e| format!("Invalid hostname: {}", e))?;
            }
            "port" => {
                let port = if value.is_empty() {
                    None
                } else {
                    Some(value.parse::<u16>()
                        .map_err(|_| "Invalid port number")?)
                };
                url.set_port(port)
                    .map_err(|_| "Cannot set port")?;
            }
            "pathname" => {
                url.set_path(value);
            }
            "search" => {
                // Remove leading "?" if present
                let query = value.strip_prefix('?').unwrap_or(value);
                url.set_query(if query.is_empty() { None } else { Some(query) });
            }
            "hash" => {
                // Remove leading "#" if present
                let fragment = value.strip_prefix('#').unwrap_or(value);
                url.set_fragment(if fragment.is_empty() { None } else { Some(fragment) });
            }
            _ => return Err(format!("Unknown URL component: {}", name)),
        }

        Ok(Self::from_url(&url))
    }
}

/// URLSearchParams operations.
#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    params: Vec<(String, String)>,
}

impl SearchParams {
    /// Parse from query string (with or without leading "?").
    pub fn parse(query: &str) -> Self {
        let query = query.strip_prefix('?').unwrap_or(query);
        let params = url::form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        Self { params }
    }

    /// Create from key-value pairs.
    pub fn from_pairs(pairs: Vec<(String, String)>) -> Self {
        Self { params: pairs }
    }

    /// Append a key-value pair.
    pub fn append(&mut self, key: &str, value: &str) {
        self.params.push((key.to_string(), value.to_string()));
    }

    /// Delete all pairs with the given key.
    pub fn delete(&mut self, key: &str) {
        self.params.retain(|(k, _)| k != key);
    }

    /// Get the first value for a key.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.params.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Get all values for a key.
    pub fn get_all(&self, key: &str) -> Vec<&str> {
        self.params.iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// Check if a key exists.
    pub fn has(&self, key: &str) -> bool {
        self.params.iter().any(|(k, _)| k == key)
    }

    /// Set a key to a single value (removes existing).
    pub fn set(&mut self, key: &str, value: &str) {
        self.delete(key);
        self.append(key, value);
    }

    /// Sort params by key.
    pub fn sort(&mut self) {
        self.params.sort_by(|a, b| a.0.cmp(&b.0));
    }

    /// Get all entries as pairs.
    pub fn entries(&self) -> &[(String, String)] {
        &self.params
    }

    /// Get all keys.
    pub fn keys(&self) -> Vec<&str> {
        self.params.iter().map(|(k, _)| k.as_str()).collect()
    }

    /// Get all values.
    pub fn values(&self) -> Vec<&str> {
        self.params.iter().map(|(_, v)| v.as_str()).collect()
    }

    /// Serialize to query string (without leading "?").
    pub fn to_string(&self) -> String {
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(&self.params)
            .finish()
    }

    /// Get size (number of entries).
    pub fn size(&self) -> usize {
        self.params.len()
    }
}

/// JavaScript module code for URL and URLSearchParams classes.
pub fn url_module_js() -> &'static str {
    include_str!("url_shim.js")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_parse_full() {
        let url = UrlComponents::parse("https://user:pass@example.com:8080/path?query=1#hash", None).unwrap();
        assert_eq!(url.protocol, "https:");
        assert_eq!(url.username, "user");
        assert_eq!(url.password, "pass");
        assert_eq!(url.hostname, "example.com");
        assert_eq!(url.port, "8080");
        assert_eq!(url.pathname, "/path");
        assert_eq!(url.search, "?query=1");
        assert_eq!(url.hash, "#hash");
    }

    #[test]
    fn test_url_parse_with_base() {
        let url = UrlComponents::parse("/path", Some("https://example.com")).unwrap();
        assert_eq!(url.href, "https://example.com/path");
    }

    #[test]
    fn test_url_set_pathname() {
        let url = UrlComponents::parse("https://example.com/old", None).unwrap();
        let updated = url.set_component("pathname", "/new").unwrap();
        assert_eq!(updated.pathname, "/new");
        assert_eq!(updated.href, "https://example.com/new");
    }

    #[test]
    fn test_search_params_basic() {
        let mut params = SearchParams::parse("?a=1&b=2&a=3");
        assert_eq!(params.get("a"), Some("1"));
        assert_eq!(params.get_all("a"), vec!["1", "3"]);
        assert!(params.has("b"));

        params.set("a", "new");
        assert_eq!(params.get("a"), Some("new"));
        assert_eq!(params.get_all("a"), vec!["new"]);
    }

    #[test]
    fn test_search_params_serialize() {
        let mut params = SearchParams::default();
        params.append("key", "value with spaces");
        params.append("another", "test");
        assert_eq!(params.to_string(), "key=value+with+spaces&another=test");
    }
}
