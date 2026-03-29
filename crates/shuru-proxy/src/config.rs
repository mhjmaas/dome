use std::collections::HashMap;

/// Configuration for the proxy engine.
#[derive(Debug, Clone, Default)]
pub struct ProxyConfig {
    /// Secrets to inject. Key is the env var name visible to the guest.
    /// The guest gets a random placeholder token; the proxy substitutes
    /// the real value only when the request targets an allowed host.
    pub secrets: HashMap<String, SecretConfig>,
    /// Network access rules.
    pub network: NetworkConfig,
}

/// A secret that the proxy injects into HTTP requests.
#[derive(Debug, Clone)]
pub struct SecretConfig {
    /// Host environment variable to read the real value from.
    pub from: String,
    /// Domain patterns where this secret may be sent (e.g., "api.openai.com").
    /// The proxy only substitutes the placeholder on requests to these hosts.
    pub hosts: Vec<String>,
    /// If set, use this value directly instead of reading from the host env var.
    pub value: Option<String>,
}

/// Network access policy.
#[derive(Debug, Clone, Default)]
pub struct NetworkConfig {
    /// Allowed domain patterns. Empty = allow all.
    /// Supports wildcards: "*.openai.com", "registry.npmjs.org".
    pub allow: Vec<String>,
}

impl ProxyConfig {
    /// Check if a domain is allowed by the network policy.
    /// Empty allowlist means all domains are allowed.
    pub fn is_domain_allowed(&self, domain: &str) -> bool {
        if self.network.allow.is_empty() {
            return true;
        }
        self.network
            .allow
            .iter()
            .any(|pattern| domain_matches(pattern, domain))
    }

    /// Get all secret placeholder→real value mappings for a given domain.
    pub fn secrets_for_domain(
        &self,
        domain: &str,
        placeholders: &HashMap<String, String>,
    ) -> Vec<(String, String)> {
        let mut substitutions = Vec::new();
        for (name, secret) in &self.secrets {
            if secret
                .hosts
                .iter()
                .any(|pattern| domain_matches(pattern, domain))
            {
                if let Some(placeholder) = placeholders.get(name) {
                    let real_value = secret
                        .value
                        .clone()
                        .or_else(|| std::env::var(&secret.from).ok());
                    if let Some(real_value) = real_value {
                        substitutions.push((placeholder.clone(), real_value));
                    }
                }
            }
        }
        substitutions
    }
}

/// Simple wildcard domain matching.
/// "*.example.com" matches "api.example.com" but not "example.com".
/// "example.com" matches exactly "example.com".
fn domain_matches(pattern: &str, domain: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix("*.") {
        domain.ends_with(suffix) && domain.len() > suffix.len() && domain.as_bytes()[domain.len() - suffix.len() - 1] == b'.'
    } else {
        pattern == domain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_domain_matching() {
        assert!(domain_matches("example.com", "example.com"));
        assert!(!domain_matches("example.com", "api.example.com"));
        assert!(domain_matches("*.example.com", "api.example.com"));
        assert!(domain_matches("*.example.com", "deep.api.example.com"));
        assert!(!domain_matches("*.example.com", "example.com"));
        assert!(!domain_matches("*.example.com", "notexample.com"));
    }
}
