use clap::Parser;
use kvs::Result;
use kvs::network::{Request, Response, decode_response, read_frame, write_request};
use kvs::{KvsError, ReplCommand};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use std::io;
use std::net::{SocketAddr, TcpStream};

#[derive(Parser)]
#[command(name = "kvs-client", about = "Bitcask key/value client")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:4000")]
    addr: SocketAddr,
}

fn main() {
    let args = Args::parse();

    let mut stream = match TcpStream::connect(args.addr) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("Error connecting to {}: {}", args.addr, err);
            return;
        }
    };

    let mut rl = DefaultEditor::new().unwrap();

    kvs::print_help();

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
                        Some(ReplCommand::Set { key, value }) => set(key, value, &mut stream),
                        Some(ReplCommand::Get { key }) => get(key, &mut stream),
                        Some(ReplCommand::Rm { key }) => rm(key, &mut stream),
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

fn set(key: &str, value: &str, stream: &mut TcpStream) {
    match call(stream, &Request::Set { key, value }) {
        Ok(Response::Ok) => println!("SET {} = {}", key, value),
        Ok(other) => eprintln!("Error: unexpected response {:?}", other),
        Err(err) => eprintln!("Error: {}", err),
    }
}

fn get(key: &str, stream: &mut TcpStream) {
    match call(stream, &Request::Get { key }) {
        Ok(Response::Value(value)) => println!("{}", value),
        Ok(other) => eprintln!("Error: unexpected response {:?}", other),
        Err(KvsError::KeyNotFound) => println!("Key not found"),
        Err(err) => eprintln!("Error: {}", err),
    }
}

fn rm(key: &str, stream: &mut TcpStream) {
    match call(stream, &Request::Rm { key }) {
        Ok(Response::Ok) => println!("Removed key: {}", key),
        Ok(other) => eprintln!("Error: unexpected response {:?}", other),
        Err(err) => eprintln!("Error: {}", err),
    }
}

fn call(stream: &mut TcpStream, req: &Request) -> Result<Response> {
    write_request(stream, req)?;
    let (msg_type, payload) = read_frame(stream)?.ok_or_else(|| {
        KvsError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "server closed the connection",
        ))
    })?;
    match decode_response(msg_type, &payload)? {
        Response::Error(code) => Err(code.into()),
        resp => Ok(resp),
    }
}
