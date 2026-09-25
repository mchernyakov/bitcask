use clap::Parser;
use kvs::network::{ErrorCode, Response, decode_request, execute, read_frame, write_response};
use kvs::{Config, KvStore};
use kvs::{Result, Store, StoreType};
use log::{error, info};
use std::io::{BufReader, BufWriter};
use std::net::{SocketAddr, TcpListener, TcpStream};

#[derive(Parser)]
#[command(name = "kvs-server", about = "Bitcask key/value server")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:4000")]
    addr: SocketAddr,

    #[arg(long, default_value = "kvs/")]
    dir: String,

    #[arg(long, value_enum, default_value_t = StoreType::Bitcask)]
    store_type: StoreType,
}

fn main() -> kvs::Result<()> {
    kvs::log::configure_logger();

    let args = Args::parse();

    let store = Store::open(Config::new(&args.dir, args.store_type))?;

    let listener = TcpListener::bind(args.addr)?;
    info!(
        "start server: kvs-server --addr {} --dir {} --store-type {:?}",
        args.addr, args.dir, args.store_type
    );

    for stream in listener.incoming() {
        let stream = stream?;
        let store = store.clone();
        std::thread::Builder::new()
            .name(format!("conn-{}", stream.peer_addr()?))
            .spawn(move || {
                if let Err(e) = handle(stream, store) {
                    error!("connection error: {}", e);
                }
            })?;
    }

    Ok(())
}

fn handle<S: KvStore>(stream: TcpStream, store: S) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    while let Some((msg_type, payload)) = read_frame(&mut reader)? {
        let resp = match decode_request(msg_type, &payload) {
            Ok(req) => execute(&store, req),
            Err(_) => Response::Error(ErrorCode::InvalidRequest),
        };
        write_response(&mut writer, &resp)?;
    }
    Ok(())
}
