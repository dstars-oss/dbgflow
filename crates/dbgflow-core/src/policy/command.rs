use crate::{DbgFlowError, Result};

#[derive(Debug, Clone)]
pub struct CommandPolicy {
    allowed_prefixes: Vec<String>,
    denied_prefixes: Vec<String>,
}

impl CommandPolicy {
    pub fn default_query_policy() -> Self {
        Self {
            allowed_prefixes: vec![
                "!analyze -v".to_string(),
                "k".to_string(),
                "kb".to_string(),
                "kv".to_string(),
                "~* k".to_string(),
                "lm".to_string(),
                "r".to_string(),
                ".ecxr".to_string(),
                ".exr".to_string(),
                ".cxr".to_string(),
                ".reload".to_string(),
                ".sympath".to_string(),
                "dx".to_string(),
            ],
            denied_prefixes: vec![
                ".shell".to_string(),
                ".load".to_string(),
                ".scriptload".to_string(),
                "g".to_string(),
                "p".to_string(),
                "t".to_string(),
            ],
        }
    }

    pub fn check_command(&self, command: &str) -> Result<()> {
        let normalized = command.trim();

        if normalized.is_empty() {
            return Err(DbgFlowError::CommandDenied {
                command: command.to_string(),
                reason: "empty command".to_string(),
            });
        }

        if self
            .denied_prefixes
            .iter()
            .any(|prefix| has_command_prefix(normalized, prefix))
        {
            return Err(DbgFlowError::CommandDenied {
                command: command.to_string(),
                reason: "command is explicitly denied".to_string(),
            });
        }

        if self
            .allowed_prefixes
            .iter()
            .any(|prefix| has_command_prefix(normalized, prefix))
        {
            return Ok(());
        }

        Err(DbgFlowError::CommandDenied {
            command: command.to_string(),
            reason: "command is not allowlisted".to_string(),
        })
    }
}

fn has_command_prefix(command: &str, prefix: &str) -> bool {
    command == prefix
        || command
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

#[cfg(test)]
mod tests {
    use super::CommandPolicy;

    #[test]
    fn allows_allowlisted_query_commands() {
        let policy = CommandPolicy::default_query_policy();

        policy.check_command("!analyze -v").expect("allow analyze");
        policy.check_command("kv 20").expect("allow stack query");
    }

    #[test]
    fn denies_dangerous_or_unknown_commands() {
        let policy = CommandPolicy::default_query_policy();

        assert!(policy.check_command(".shell dir").is_err());
        assert!(policy.check_command("g").is_err());
        assert!(policy.check_command("unknown").is_err());
    }
}
