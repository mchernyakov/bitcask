use kvs::Bitcask;
use kvs::DurabilityPolicy;
use kvs::KvStore;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

fn main() {
    let mut rl = DefaultEditor::new().unwrap();

    print_help();

    let mut kvstore = match Bitcask::open("kvs/", DurabilityPolicy::OsDecides) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("Error opening KvStore (Bitcask): {}", err);
            return;
        }
    };

    loop {
        match rl.readline("kvs> ") {
            Ok(line) => {
                let line = line.trim();

                if line.is_empty() {
                    continue;
                }

                rl.add_history_entry(line).unwrap();

                match line {
                    "quit" | "exit" => break,
                    "help" => {
                        print_help();
                    }
                    _ => {
                        execute_command(line, &mut kvstore);
                    }
                }
            }

            // Ctrl-D
            Err(ReadlineError::Eof) => {
                break;
            }

            // Ctrl-C
            Err(ReadlineError::Interrupted) => {
                println!("^C");
                continue;
            }

            Err(err) => {
                eprintln!("Error: {}", err);
                break;
            }
        }
    }
}

fn execute_command(input: &str, kvstore: &mut dyn KvStore) {
    let mut parts = input.split_whitespace();

    let command = match parts.next() {
        Some(command) => command,
        None => return,
    };

    match command {
        "set" => {
            let key = match parts.next() {
                Some(key) => key,
                None => {
                    eprintln!("Usage: set <KEY> <VALUE>");
                    return;
                }
            };

            let value = match parts.next() {
                Some(value) => value,
                None => {
                    eprintln!("Usage: set <KEY> <VALUE>");
                    return;
                }
            };

            set(key, value, kvstore);
        }

        "get" => {
            let key = match parts.next() {
                Some(key) => key,
                None => {
                    eprintln!("Usage: get <KEY>");
                    return;
                }
            };

            get(key, kvstore);
        }

        "rm" => {
            let key = match parts.next() {
                Some(key) => key,
                None => {
                    eprintln!("Usage: rm <KEY>");
                    return;
                }
            };

            rm(key, kvstore);
        }

        _ => {
            eprintln!("Unknown command: {}", command);
            eprintln!("Type 'help' for available commands.");
        }
    }
}

fn print_help() {
    println!("Available commands:");
    println!("  set <KEY> <VALUE>  Set the value of a key");
    println!("  get <KEY>          Get the value of a key");
    println!("  rm <KEY>           Remove a key");
    println!("  help               Show this help");
    println!("  quit               Exit");
}

fn set(key: &str, value: &str, kvstore: &mut dyn KvStore) {
    let res = kvstore.set(key, value);
    if let Err(e) = res {
        eprintln!("Error: {}", e);
    } else {
        println!("SET {} = {}", key, value);
    }
}

fn get(key: &str, kvstore: &mut dyn KvStore) {
    let res = kvstore.get(key);
    match res {
        Ok(Some(value)) => println!("{}", value),
        Ok(None) => println!("Key not found"),
        Err(err) => eprintln!("Error: {}", err),
    }
}

fn rm(key: &str, kvstore: &mut dyn KvStore) {
    if let Err(err) = kvstore.remove(key) {
        eprintln!("Error: {}", err);
    } else {
        println!("Removed key: {}", key);
    }
}
