use flow::prelude::*;
use flow;

use bincode;
use bufstream::BufStream;
use std::io::prelude::*;
use std::io;
use std::sync::{Arc, Mutex};

use vec_map::VecMap;

use std::net::TcpListener;
use std::net::TcpStream;
use std::thread;

/// Available RPC methods
#[derive(Serialize, Deserialize)]
pub enum Method {
    /// Query the given `view` for all records whose key column matches the given value.
    Query {
        /// The view to query
        view: usize,
        /// The key value to use for the given query's free parameter
        key: DataType,
    },

    /// Obtain a MutatorBuilder for the indicated view.
    GetMutatorBuilder {
        /// The view to get a mutator builder for.
        view: usize,
    },

    /// Flush any buffered responses.
    Flush,
}

/// Construct a new `Server` handle for all Soup endpoints
pub fn make_server(soup: &flow::Blender) -> Server {
    // Figure out what inputs and outputs to expose
    let ins = soup.inputs()
        .into_iter()
        .map(|(ni, n)| {
            (
                ni.index(),
                (
                    n.name().to_owned(),
                    n.fields().iter().cloned().collect(),
                    soup.get_mutator_builder(ni),
                ),
            )
        })
        .collect();
    /*let outs = soup.outputs()
        .into_iter()
        .map(|(ni, n)| {
            (
                ni.index(),
                (
                    n.name().to_owned(),
                    n.fields().iter().cloned().collect(),
                    soup.get_getter(ni).unwrap(),
                ),
            )
        })
        .collect();*/
    let outs = VecMap::new();

    Server {
        put: ins,
        get: outs,
    }
}

/// A handle to Soup put and get endpoints
pub struct Server {
    /// All put endpoints.
    pub put: VecMap<(String, Vec<String>, flow::MutatorBuilder)>,
    /// All get endpoints.
    pub get: VecMap<(String, Vec<String>, flow::Getter)>,
}

/// Handle RPCs from a single `TcpStream`
pub fn main(stream: TcpStream, s: Server) {
    let mut stream = BufStream::new(stream);
    loop {
        match bincode::deserialize_from(&mut stream, bincode::Infinite) {
            Ok(Method::Query { view, key }) => {
                let r = s.get[view]
                    .2
                    .lookup_map(
                        &key,
                        |rs| {
                            bincode::serialize_into(
                                &mut stream,
                                &Ok::<_, ()>(rs),
                                bincode::Infinite,
                            )
                        },
                        true,
                    )
                    .map(|r| r.unwrap())
                    .unwrap_or_else(|e| {
                        bincode::serialize_into(
                            &mut stream,
                            &Err::<&[Arc<Vec<DataType>>], _>(e),
                            bincode::Infinite,
                        )
                    });

                if let Err(e) = r {
                    println!("client left prematurely: {:?}", e);
                    break;
                }
            }
            Ok(Method::GetMutatorBuilder {view}) => {
                let r = bincode::serialize_into(&mut stream, &s.put[view].2, bincode::Infinite);
                if let Err(e) = r {
                    println!("client left prematurely: {:?}", e);
                    break;
                }
            }
            Ok(Method::Flush) => {
                if let Err(e) = stream.flush() {
                    println!("client left prematurely: {:?}", e);
                    break;
                }
            }
            Err(e) => {
                match *e {
                    bincode::internal::ErrorKind::IoError(e) => {
                        if e.kind() != io::ErrorKind::UnexpectedEof {
                            println!("client left: {:?}", e);
                        }
                    }
                    e => {
                        println!("client sent bad request: {:?}", e);
                    }
                }
                break;
            }
        }
    }
}

/// Starts a server which allows read/write access to the Soup using a binary protocol.
///
/// In particular, requests should all be of the form `types::Request`
pub fn run<T: Into<::std::net::SocketAddr>>(soup: Arc<Mutex<flow::Blender>>, addr: T) {
    let listener = TcpListener::bind(addr.into()).unwrap();

    // Figure out what inputs and outputs to expose
    let mut i = 0;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let g = soup.lock().unwrap();
                println!("{}", g);
                let s = make_server(&g);
                thread::Builder::new()
                    .name(format!("rpc{}", i))
                    .spawn(move || {
                        stream.set_nodelay(true).unwrap();
                        main(stream, s);
                    })
                    .unwrap();
                i += 1;
            }
            Err(e) => {
                print!("accept failed {:?}\n", e);
            }
        }
    }
}
