use clap::Parser;
use kvs::network::{
    ErrorCode, RespCommand, RespRequest, Response, decode_error_message, decode_request,
    decode_request_resp, execute, is_resp, read_frame, read_frame_resp, write_config_get,
    write_error, write_response, write_response_resp,
};
use kvs::{Config, KvStore};
use kvs::{Result, Store, StoreType};
use log::{error, info};
use std::io::{BufRead, BufReader, BufWriter, Write};
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

    let first = {
        let buf = reader.fill_buf()?;

        match buf.first() {
            Some(&b) => b,
            None => return Ok(()), // connection closed
        }
    };

    if is_resp(first) {
        return handle_resp(reader, writer, store);
    }

    // custom protocol
    while let Some((msg_type, payload)) = read_frame(&mut reader)? {
        let resp = match decode_request(msg_type, &payload) {
            Ok(req) => execute(&store, req),
            Err(_) => Response::Error(ErrorCode::InvalidRequest),
        };
        write_response(&mut writer, &resp)?;
    }
    Ok(())
}

fn handle_resp<S: KvStore>(
    mut reader: BufReader<TcpStream>,
    mut writer: BufWriter<TcpStream>,
    store: S,
) -> Result<()> {
    while let Some(frame) = read_frame_resp(&mut reader)? {
        match decode_request_resp(&frame) {
            Ok(RespRequest::Kv(req)) => {
                let cmd = RespCommand::from(&req);
                let resp = execute(&store, req);
                write_response_resp(&mut writer, cmd, &resp)?;
            }
            Ok(RespRequest::ConfigGet(names)) => write_config_get(&mut writer, &names)?,
            Err(e) => write_error(&mut writer, &decode_error_message(&e))?,
        }
        if reader.buffer().is_empty() {
            writer.flush()?;
        }
    }
    writer.flush()?;
    Ok(())
}
