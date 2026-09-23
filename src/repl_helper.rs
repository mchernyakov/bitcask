use log::error;

#[derive(Debug, PartialEq, Eq)]
pub enum ReplCommand<'a> {
    Set { key: &'a str, value: &'a str },
    Get { key: &'a str },
    Rm { key: &'a str },
}

pub fn parse_command(input: &str) -> Option<ReplCommand<'_>> {
    let mut parts = input.split_whitespace();
    let command = parts.next()?;

    match command {
        "set" => match (parts.next(), parts.next()) {
            (Some(key), Some(value)) => Some(ReplCommand::Set { key, value }),
            _ => {
                error!("Usage: set <KEY> <VALUE>");
                None
            }
        },

        "get" => match parts.next() {
            Some(key) => Some(ReplCommand::Get { key }),
            None => {
                error!("Usage: get <KEY>");
                None
            }
        },

        "rm" => match parts.next() {
            Some(key) => Some(ReplCommand::Rm { key }),
            None => {
                error!("Usage: rm <KEY>");
                None
            }
        },

        _ => {
            error!("Unknown command: {}", command);
            error!("Type 'help' for available commands.");
            None
        }
    }
}

pub fn print_help() {
    println!("Available commands:");
    println!("  set <KEY> <VALUE>  Set the value of a key");
    println!("  get <KEY>          Get the value of a key");
    println!("  rm <KEY>           Remove a key");
    println!("  help               Show this help");
    println!("  quit               Exit");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_commands() {
        assert_eq!(
            parse_command("set k v"),
            Some(ReplCommand::Set {
                key: "k",
                value: "v"
            })
        );
        assert_eq!(parse_command("get k"), Some(ReplCommand::Get { key: "k" }));
        assert_eq!(parse_command("rm k"), Some(ReplCommand::Rm { key: "k" }));
        assert_eq!(
            parse_command("  set   k   v  "),
            Some(ReplCommand::Set {
                key: "k",
                value: "v"
            })
        );
    }

    #[test]
    fn rejects_missing_args_and_unknown_commands() {
        assert_eq!(parse_command(""), None);
        assert_eq!(parse_command("set"), None);
        assert_eq!(parse_command("set k"), None);
        assert_eq!(parse_command("get"), None);
        assert_eq!(parse_command("rm"), None);
        assert_eq!(parse_command("nope k"), None);
    }
}
