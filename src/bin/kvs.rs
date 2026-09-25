use clap::Parser;
use kvs::Config;
use kvs::KvStore;
use kvs::ReplCommand;
use kvs::{Store, StoreType};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

#[derive(Parser)]
#[command(name = "kvs-repl", about = "Bitcask key/value REPL")]
struct Args {
    #[arg(long, default_value = "kvs/")]
    dir: String,

    #[arg(long, value_enum, default_value_t = StoreType::Bitcask)]
    store_type: StoreType,
}

fn main() {
    kvs::log::configure_logger();

    let args = Args::parse();

    let mut rl = DefaultEditor::new().unwrap();

    kvs::print_help();

    let kvstore = match Store::open(Config::new(&args.dir, args.store_type)) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("Error opening KvStore: {}", err);
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
                        kvs::print_help();
                    }
                    _ => match kvs::parse_command(line) {
                        Some(ReplCommand::Set { key, value }) => set(key, value, &kvstore),
                        Some(ReplCommand::Get { key }) => get(key, &kvstore),
                        Some(ReplCommand::Rm { key }) => rm(key, &kvstore),
                        None => {}
                    },
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

fn set(key: &str, value: &str, kvstore: &impl KvStore) {
    let res = kvstore.set(key, value);
    if let Err(e) = res {
        eprintln!("Error: {}", e);
    } else {
        println!("SET {} = {}", key, value);
    }
}

fn get(key: &str, kvstore: &impl KvStore) {
    let res = kvstore.get(key);
    match res {
        Ok(Some(value)) => println!("{}", value),
        Ok(None) => println!("Key not found"),
        Err(err) => eprintln!("Error: {}", err),
    }
}

fn rm(key: &str, kvstore: &impl KvStore) {
    if let Err(err) = kvstore.remove(key) {
        eprintln!("Error: {}", err);
    } else {
        println!("Removed key: {}", key);
    }
}
