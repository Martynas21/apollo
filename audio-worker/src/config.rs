// Deployment contract with `apollo`'s own default in `src/config.rs` — both
// processes fall back to this same value when `AUDIO_WORKER_SOCKET` is
// unset, matching compose.yaml's service name. Keep the two in sync if it
// ever changes.
const DEFAULT_AUDIO_WORKER_SOCKET: &str = "audio-worker:7878";

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
}

impl Config {
    pub fn from_env() -> Self {
        Self::from_source(|key| std::env::var(key))
    }

    fn from_source(lookup: impl Fn(&str) -> Result<String, std::env::VarError>) -> Self {
        Self {
            bind_addr: optional_env_var(&lookup, "AUDIO_WORKER_SOCKET")
                .unwrap_or_else(|| DEFAULT_AUDIO_WORKER_SOCKET.to_string()),
        }
    }
}

fn optional_env_var(
    lookup: &impl Fn(&str) -> Result<String, std::env::VarError>,
    key: &str,
) -> Option<String> {
    lookup(key).ok().filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::Config;
    use std::collections::HashMap;
    use std::env::VarError;

    fn lookup<'a>(vars: &'a HashMap<&str, &str>) -> impl Fn(&str) -> Result<String, VarError> + 'a {
        move |key| {
            vars.get(key)
                .map(|v| v.to_string())
                .ok_or(VarError::NotPresent)
        }
    }

    #[test]
    fn bind_addr_absent_uses_default() {
        let vars = HashMap::new();
        let config = Config::from_source(lookup(&vars));
        assert_eq!(config.bind_addr, "audio-worker:7878");
    }

    #[test]
    fn bind_addr_present_overrides_default() {
        let mut vars = HashMap::new();
        vars.insert("AUDIO_WORKER_SOCKET", "127.0.0.1:7878");
        let config = Config::from_source(lookup(&vars));
        assert_eq!(config.bind_addr, "127.0.0.1:7878");
    }

    #[test]
    fn bind_addr_empty_string_uses_default() {
        let mut vars = HashMap::new();
        vars.insert("AUDIO_WORKER_SOCKET", "");
        let config = Config::from_source(lookup(&vars));
        assert_eq!(config.bind_addr, "audio-worker:7878");
    }
}
