extern crate undermoon;
extern crate tokio;
extern crate futures;

use futures::{Future, Stream};
use tokio::net::TcpListener;
use undermoon::proxy::session::{Session, handle_conn};
use undermoon::proxy::executor::SharedForwardHandler;

fn main() {
    let addr = "127.0.0.1:5299".parse().unwrap();
    let listener = TcpListener::bind(&addr)
        .expect("unable to bind TCP listener");

    let forward_handler = SharedForwardHandler::new();

    let server = listener.incoming()
        .map_err(|e| eprintln!("accept failed = {:?}", e))
        .for_each(move |sock| {
            println!("accept conn {:?}", sock);
            let handle_clone = forward_handler.clone();
            let handle_conn = handle_conn(Session::new(handle_clone), sock)
                .map_err(|err| {
                    eprintln!("IO error {:?}", err)
                });
            tokio::spawn(handle_conn)
        });

    tokio::run(server);
}