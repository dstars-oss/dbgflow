use crate::{DbgFlowError, Result};

#[derive(Debug, Clone)]
pub struct CommandPolicy {
    denied_prefixes: Vec<String>,
}

impl CommandPolicy {
    pub fn default_query_policy() -> Self {
        Self {
            denied_prefixes: vec![
                ".shell".to_string(),
                ".load".to_string(),
                ".loadby".to_string(),
                ".scriptload".to_string(),
                ".scriptrun".to_string(),
                ".dump".to_string(),
                ".writemem".to_string(),
                "$<".to_string(),
                "$><".to_string(),
                "$$<".to_string(),
                "$$><".to_string(),
                "$$>a<".to_string(),
                "as".to_string(),
            ],
        }
    }

    pub fn is_run_control_command(&self, command: &str) -> bool {
        let normalized = command.trim();
        matches!(normalized, "g" | "p" | "t")
            || has_command_prefix(normalized, "g")
            || has_command_prefix(normalized, "p")
            || has_command_prefix(normalized, "t")
    }

    pub fn check_command(&self, command: &str) -> Result<()> {
        let normalized = command.trim();

        if normalized.is_empty() {
            return Err(DbgFlowError::CommandDenied {
                command: command.to_string(),
                reason: "empty command".to_string(),
            });
        }
        if contains_command_separator(normalized) {
            return Err(DbgFlowError::CommandDenied {
                command: command.to_string(),
                reason: "command separators are not allowed".to_string(),
            });
        }

        let lowered = normalized.to_ascii_lowercase();
        if lowered.starts_with('$') {
            return Err(DbgFlowError::CommandDenied {
                command: command.to_string(),
                reason: "dollar-prefixed debugger metacommands are denied".to_string(),
            });
        }
        if contains_fixed_alias_reference(&lowered) {
            return Err(DbgFlowError::CommandDenied {
                command: command.to_string(),
                reason: "fixed-name aliases are denied".to_string(),
            });
        }
        if defines_fixed_alias(&lowered) {
            return Err(DbgFlowError::CommandDenied {
                command: command.to_string(),
                reason: "fixed-name alias definition is denied".to_string(),
            });
        }
        if self
            .denied_prefixes
            .iter()
            .any(|prefix| has_command_prefix(&lowered, prefix))
        {
            return Err(DbgFlowError::CommandDenied {
                command: command.to_string(),
                reason: "command is explicitly denied".to_string(),
            });
        }

        Ok(())
    }
}

fn contains_command_separator(command: &str) -> bool {
    command
        .chars()
        .any(|ch| matches!(ch, ';' | '\r' | '\n' | '\u{2028}' | '\u{2029}'))
}

fn contains_fixed_alias_reference(command: &str) -> bool {
    (0..=9).any(|index| command.contains(&format!("$u{index}")))
}

fn defines_fixed_alias(command: &str) -> bool {
    let Some(rest) = command.strip_prefix('r') else {
        return false;
    };
    let rest = rest.trim_start();
    (0..=9).any(|index| {
        rest.strip_prefix(&format!(".u{index}"))
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    })
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
    fn allows_non_denied_debugger_commands() {
        let policy = CommandPolicy::default_query_policy();

        policy.check_command("!analyze -v").expect("allow analyze");
        policy.check_command("kv 20").expect("allow stack query");
        policy.check_command("g").expect("allow go");
        policy.check_command("dt ntdll!_PEB").expect("allow dt");
        policy.check_command("!avrf").expect("allow verifier query");
        policy
            .check_command(".sympath+ C:\\symbols")
            .expect("allow sympath append");
    }

    #[test]
    fn denies_dangerous_or_unknown_commands() {
        let policy = CommandPolicy::default_query_policy();

        assert!(policy.check_command(".shell dir").is_err());
        assert!(policy.check_command(".load evil.dll").is_err());
        assert!(policy.check_command(".scriptload evil.js").is_err());
        assert!(policy.check_command(".dump /ma C:\\temp\\x.dmp").is_err());
        assert!(policy.check_command("$>< C:\\temp\\commands.txt").is_err());
        assert!(policy.check_command("$><C:\\temp\\commands.txt").is_err());
        assert!(policy
            .check_command("$$>a< C:\\temp\\commands.txt")
            .is_err());
        assert!(policy
            .check_command("$$>a<C:\\temp\\commands.txt arg")
            .is_err());
        assert!(policy.check_command("as x .shell dir").is_err());
        assert!(policy.check_command("r .u0 = .shell dir").is_err());
        assert!(policy.check_command("k $u0").is_err());
        assert!(policy.check_command("k; .shell dir").is_err());
        assert!(policy.check_command("k\n.shell dir").is_err());
        assert!(policy.check_command("unknown").is_ok());
    }
}
