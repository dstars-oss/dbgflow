use crate::{DbgFlowError, Result};
use std::collections::{BTreeMap, HashMap};
use std::fmt;

const SYMBOL_PROXY_KEY: &str = "_NT_SYMBOL_PROXY";
const HTTP_PROXY_KEY: &str = "HTTP_PROXY";
const HTTPS_PROXY_KEY: &str = "HTTPS_PROXY";
const ALL_PROXY_KEY: &str = "ALL_PROXY";
const NO_PROXY_KEY: &str = "NO_PROXY";
const LOWER_HTTP_PROXY_KEY: &str = "http_proxy";
const LOWER_HTTPS_PROXY_KEY: &str = "https_proxy";
const LOWER_ALL_PROXY_KEY: &str = "all_proxy";
const LOWER_NO_PROXY_KEY: &str = "no_proxy";

const ALL_PROXY_KEYS: &[&str] = &[
    SYMBOL_PROXY_KEY,
    HTTP_PROXY_KEY,
    HTTPS_PROXY_KEY,
    ALL_PROXY_KEY,
    NO_PROXY_KEY,
    LOWER_HTTP_PROXY_KEY,
    LOWER_HTTPS_PROXY_KEY,
    LOWER_ALL_PROXY_KEY,
    LOWER_NO_PROXY_KEY,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxySource {
    Cli,
    Environment,
    Disabled,
    None,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProxyEnvironment {
    source: ProxySource,
    vars: BTreeMap<String, String>,
}

impl ProxyEnvironment {
    pub fn none() -> Self {
        Self {
            source: ProxySource::None,
            vars: BTreeMap::new(),
        }
    }

    pub fn disabled() -> Self {
        Self {
            source: ProxySource::Disabled,
            vars: BTreeMap::new(),
        }
    }

    pub fn from_cli_proxy_url(value: &str) -> Result<Self> {
        validate_proxy_value(value, "--proxy-url")?;
        let symbol_proxy = symbol_proxy_from_url(value)?;
        let mut vars = BTreeMap::new();
        vars.insert(SYMBOL_PROXY_KEY.to_string(), symbol_proxy);
        vars.insert(HTTP_PROXY_KEY.to_string(), value.to_string());
        vars.insert(HTTPS_PROXY_KEY.to_string(), value.to_string());
        vars.insert(LOWER_HTTP_PROXY_KEY.to_string(), value.to_string());
        vars.insert(LOWER_HTTPS_PROXY_KEY.to_string(), value.to_string());
        Ok(Self {
            source: ProxySource::Cli,
            vars,
        })
    }

    pub fn from_current_environment() -> Result<Self> {
        let env = std::env::vars().collect::<HashMap<_, _>>();
        Self::from_environment_map(&env)
    }

    pub fn from_environment_map(env: &HashMap<String, String>) -> Result<Self> {
        let mut vars = BTreeMap::new();
        insert_env_pair(env, &mut vars, SYMBOL_PROXY_KEY, None)?;
        insert_env_pair(env, &mut vars, HTTP_PROXY_KEY, Some(LOWER_HTTP_PROXY_KEY))?;
        insert_env_pair(env, &mut vars, HTTPS_PROXY_KEY, Some(LOWER_HTTPS_PROXY_KEY))?;
        insert_env_pair(env, &mut vars, ALL_PROXY_KEY, Some(LOWER_ALL_PROXY_KEY))?;
        insert_env_pair(env, &mut vars, NO_PROXY_KEY, Some(LOWER_NO_PROXY_KEY))?;
        if vars.is_empty() {
            Ok(Self::none())
        } else {
            Ok(Self {
                source: ProxySource::Environment,
                vars,
            })
        }
    }

    pub fn source(&self) -> ProxySource {
        self.source
    }

    pub fn value_for(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }

    pub fn env_vars(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    pub fn removed_keys(&self) -> Vec<&'static str> {
        ALL_PROXY_KEYS
            .iter()
            .copied()
            .filter(|key| !self.vars.contains_key(*key))
            .collect()
    }

    pub fn proxy_keys(&self) -> Vec<&'static str> {
        [
            SYMBOL_PROXY_KEY,
            HTTP_PROXY_KEY,
            HTTPS_PROXY_KEY,
            ALL_PROXY_KEY,
            NO_PROXY_KEY,
        ]
        .iter()
        .copied()
        .filter(|key| self.vars.contains_key(*key))
        .collect()
    }
}

impl Default for ProxyEnvironment {
    fn default() -> Self {
        Self::none()
    }
}

impl fmt::Debug for ProxyEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyEnvironment")
            .field("source", &self.source)
            .field("proxy_keys", &self.proxy_keys())
            .finish()
    }
}

fn insert_env_pair(
    env: &HashMap<String, String>,
    vars: &mut BTreeMap<String, String>,
    upper: &'static str,
    lower: Option<&'static str>,
) -> Result<()> {
    let value = env
        .get(upper)
        .or_else(|| lower.and_then(|lower| env.get(lower)));
    let Some(value) = value else {
        return Ok(());
    };
    validate_proxy_value(value, upper)?;
    vars.insert(upper.to_string(), value.clone());
    if let Some(lower) = lower {
        vars.insert(lower.to_string(), value.clone());
    }
    Ok(())
}

fn validate_proxy_value(value: &str, label: &str) -> Result<()> {
    if value.is_empty() {
        return Err(DbgFlowError::Backend(format!("{label} must not be empty")));
    }
    if value
        .chars()
        .any(|ch| matches!(ch, '\0' | '\r' | '\n' | '\u{2028}' | '\u{2029}') || ch.is_control())
    {
        return Err(DbgFlowError::Backend(format!(
            "{label} contains unsupported control characters"
        )));
    }
    Ok(())
}

fn symbol_proxy_from_url(value: &str) -> Result<String> {
    if value.chars().any(char::is_whitespace) {
        return Err(DbgFlowError::Backend(
            "--proxy-url must not contain whitespace".to_string(),
        ));
    }
    let rest = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .ok_or_else(|| DbgFlowError::Backend("--proxy-url must use http:// or https://".into()))?;
    if rest.contains('?') || rest.contains('#') {
        return Err(DbgFlowError::Backend(
            "--proxy-url must not include query or fragment".to_string(),
        ));
    }
    if rest.contains('/') {
        return Err(DbgFlowError::Backend(
            "--proxy-url must not include a path".to_string(),
        ));
    }
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.is_empty() {
        return Err(DbgFlowError::Backend(
            "--proxy-url must include host and port".to_string(),
        ));
    }
    if authority.contains('@') {
        return Err(DbgFlowError::Backend(
            "--proxy-url credentials are not supported for _NT_SYMBOL_PROXY".to_string(),
        ));
    }
    let (host, port) = split_authority_host_port(authority)?;
    if host.is_empty() || port.is_empty() {
        return Err(DbgFlowError::Backend(
            "--proxy-url must include host and port".to_string(),
        ));
    }
    validate_proxy_port(port)?;
    Ok(authority.to_string())
}

fn validate_proxy_port(port: &str) -> Result<()> {
    match port.parse::<u16>() {
        Ok(0) => Err(DbgFlowError::Backend(
            "--proxy-url port must be a nonzero u16".to_string(),
        )),
        Ok(_) => Ok(()),
        Err(_) => Err(DbgFlowError::Backend(
            "--proxy-url port must be a nonzero u16".to_string(),
        )),
    }
}

fn split_authority_host_port(authority: &str) -> Result<(&str, &str)> {
    if let Some(bracketed_host) = authority.strip_prefix('[') {
        let Some(end) = bracketed_host.find(']') else {
            return Err(DbgFlowError::Backend(
                "--proxy-url must include host and port".to_string(),
            ));
        };
        let host = &bracketed_host[..end];
        let port = bracketed_host[end + 1..].strip_prefix(':').unwrap_or("");
        if port.contains(':') {
            return Err(DbgFlowError::Backend(
                "--proxy-url must include host and port".to_string(),
            ));
        }
        return Ok((host, port));
    }
    let (host, port) = authority.rsplit_once(':').ok_or_else(|| {
        DbgFlowError::Backend("--proxy-url must include host and port".to_string())
    })?;
    if host.contains(':') {
        return Err(DbgFlowError::Backend(
            "--proxy-url must include host and port".to_string(),
        ));
    }
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn proxy_url_sets_symsrv_and_http_proxy_vars() {
        let proxy =
            ProxyEnvironment::from_cli_proxy_url("http://127.0.0.1:7897").expect("parse proxy");

        assert_eq!(proxy.source(), ProxySource::Cli);
        assert_eq!(
            proxy.value_for("_NT_SYMBOL_PROXY").as_deref(),
            Some("127.0.0.1:7897")
        );
        assert_eq!(
            proxy.value_for("HTTP_PROXY").as_deref(),
            Some("http://127.0.0.1:7897")
        );
        assert_eq!(
            proxy.value_for("HTTPS_PROXY").as_deref(),
            Some("http://127.0.0.1:7897")
        );
        assert_eq!(
            proxy.value_for("http_proxy").as_deref(),
            Some("http://127.0.0.1:7897")
        );
        assert_eq!(
            proxy.value_for("https_proxy").as_deref(),
            Some("http://127.0.0.1:7897")
        );
        assert!(proxy.value_for("ALL_PROXY").is_none());
        assert_eq!(
            proxy.proxy_keys(),
            vec!["_NT_SYMBOL_PROXY", "HTTP_PROXY", "HTTPS_PROXY"]
        );
    }

    #[test]
    fn debug_output_redacts_proxy_values() {
        let proxy =
            ProxyEnvironment::from_cli_proxy_url("http://127.0.0.1:7897").expect("parse proxy");

        let debug = format!("{proxy:?}");

        assert!(debug.contains("_NT_SYMBOL_PROXY"));
        assert!(!debug.contains("127.0.0.1:7897"));
        assert!(!debug.contains("http://127.0.0.1:7897"));
    }

    #[test]
    fn proxy_url_accepts_bracketed_ipv6_with_explicit_port() {
        let proxy = ProxyEnvironment::from_cli_proxy_url("http://[::1]:7897").expect("parse proxy");

        assert_eq!(
            proxy.value_for("_NT_SYMBOL_PROXY").as_deref(),
            Some("[::1]:7897")
        );
    }

    #[test]
    fn environment_prefers_nt_symbol_proxy_and_uppercase_http() {
        let proxy = ProxyEnvironment::from_environment_map(&env(&[
            ("_NT_SYMBOL_PROXY", "symproxy:80"),
            ("HTTP_PROXY", "http://upper:8080"),
            ("http_proxy", "http://lower:8080"),
            ("HTTPS_PROXY", "http://secure:8080"),
            ("NO_PROXY", "localhost,127.0.0.1"),
        ]))
        .expect("read environment proxy");

        assert_eq!(proxy.source(), ProxySource::Environment);
        assert_eq!(
            proxy.value_for("_NT_SYMBOL_PROXY").as_deref(),
            Some("symproxy:80")
        );
        assert_eq!(
            proxy.value_for("HTTP_PROXY").as_deref(),
            Some("http://upper:8080")
        );
        assert_eq!(
            proxy.value_for("http_proxy").as_deref(),
            Some("http://upper:8080")
        );
        assert_eq!(
            proxy.value_for("NO_PROXY").as_deref(),
            Some("localhost,127.0.0.1")
        );
    }

    #[test]
    fn environment_rejects_empty_proxy_values() {
        assert!(ProxyEnvironment::from_environment_map(&env(&[("HTTP_PROXY", "")])).is_err());
    }

    #[test]
    fn environment_rejects_empty_uppercase_before_lowercase_fallback() {
        assert!(ProxyEnvironment::from_environment_map(&env(&[
            ("HTTP_PROXY", ""),
            ("http_proxy", "http://lower:8080")
        ]))
        .is_err());
    }

    #[test]
    fn environment_rejects_control_characters() {
        assert!(ProxyEnvironment::from_environment_map(&env(&[(
            "HTTPS_PROXY",
            "http://secure:8080\u{2028}x"
        )]))
        .is_err());
        assert!(ProxyEnvironment::from_environment_map(&env(&[(
            "HTTPS_PROXY",
            "http://secure:8080\0x"
        )]))
        .is_err());
    }

    #[test]
    fn disabled_proxy_removes_all_known_proxy_vars() {
        let proxy = ProxyEnvironment::disabled();

        assert_eq!(proxy.source(), ProxySource::Disabled);
        assert!(proxy.proxy_keys().is_empty());
        assert!(proxy.removed_keys().contains(&"_NT_SYMBOL_PROXY"));
        assert!(proxy.removed_keys().contains(&"HTTP_PROXY"));
        assert!(proxy.removed_keys().contains(&"http_proxy"));
        assert!(proxy.removed_keys().contains(&"NO_PROXY"));
    }

    #[test]
    fn rejects_invalid_cli_proxy_values() {
        assert!(ProxyEnvironment::from_cli_proxy_url("").is_err());
        assert!(ProxyEnvironment::from_cli_proxy_url("socks5://127.0.0.1:7897").is_err());
        assert!(ProxyEnvironment::from_cli_proxy_url("http://127.0.0.1").is_err());
        assert!(ProxyEnvironment::from_cli_proxy_url("http://127.0.0.1:7897\nx").is_err());
        assert!(ProxyEnvironment::from_cli_proxy_url("http://127.0.0.1:7897\u{2028}x").is_err());
        assert!(ProxyEnvironment::from_cli_proxy_url("http://:7897").is_err());
        assert!(ProxyEnvironment::from_cli_proxy_url("http://127.0.0.1:").is_err());
        assert!(ProxyEnvironment::from_cli_proxy_url("http://[::1]").is_err());
        assert!(ProxyEnvironment::from_cli_proxy_url("http://user:pass@127.0.0.1:7897").is_err());
        assert!(ProxyEnvironment::from_cli_proxy_url("http://127.0.0.1:7897?x=1").is_err());
        assert!(ProxyEnvironment::from_cli_proxy_url("http://host:7897#frag").is_err());
        assert!(ProxyEnvironment::from_cli_proxy_url("http://host:7897 ").is_err());
        assert!(ProxyEnvironment::from_cli_proxy_url("http://host:7897:extra").is_err());
        assert!(ProxyEnvironment::from_cli_proxy_url("http://host:abc").is_err());
        assert!(ProxyEnvironment::from_cli_proxy_url("http://host:65536").is_err());
        assert!(ProxyEnvironment::from_cli_proxy_url("http://host:0").is_err());
        assert!(ProxyEnvironment::from_cli_proxy_url("http://[::1]:abc").is_err());
        assert!(ProxyEnvironment::from_cli_proxy_url("http://host:7897/path").is_err());
    }
}
