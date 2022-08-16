#![feature(duration_constants)]

use std::env;
use std::error;
use std::fs;
use std::net::SocketAddr;
use std::net::UdpSocket;
use std::path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time;
use std::time::Duration;
use std::vec;

use futures::{future, prelude::*};
use moniclock::Clock;
use serde::{Deserialize, Serialize};
use tarpc::server::Channel;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Serialize, Deserialize, Clone, Copy, Debug)]
enum ServiceError {
    #[error("peer already has partner: {0:?}")]
    HasAlreadyPartner(Partner),
}

#[tarpc::service]
trait Service {
    async fn identity() -> Uuid;
    async fn epoch() -> Duration;
    async fn now() -> Duration;
    async fn recruit(partner: Partner) -> Result<Partner, ServiceError>;
}

#[derive(Clone)]
struct Oneself {
    st: Arc<Mutex<State>>,
    addr: SocketAddr,
    peers_addrs: Vec<SocketAddr>,
}

impl Oneself {
    fn new(st: State, addr: SocketAddr, peers_addrs: Vec<SocketAddr>) -> Self {
        Oneself {
            st: Arc::new(Mutex::new(st)),
            addr,
            peers_addrs,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
struct Partner {
    st: State,
    addr: SocketAddr,
}

impl From<Oneself> for Partner {
    fn from(oneself: Oneself) -> Self {
        Partner {
            st: *oneself.st.lock().unwrap(),
            addr: oneself.addr,
        }
    }
}

#[derive(Clone)]
struct Server {
    oneself: Oneself,
    partner: Option<Partner>,
}

impl Server {
    fn new(oneself: Oneself) -> Self {
        Server {
            oneself,
            partner: None,
        }
    }
}

impl Service for Server {
    type IdentityFut = future::Ready<Uuid>;
    type EpochFut = future::Ready<Duration>;
    type NowFut = future::Ready<Duration>;
    type RecruitFut = future::Ready<Result<Partner, ServiceError>>;

    fn identity(self, _ctx: tarpc::context::Context) -> Self::IdentityFut {
        future::ready(self.oneself.st.lock().unwrap().identity)
    }

    fn epoch(self, _ctx: tarpc::context::Context) -> Self::EpochFut {
        future::ready(self.oneself.st.lock().unwrap().epoch)
    }

    fn now(self, _ctx: tarpc::context::Context) -> Self::EpochFut {
        future::ready(Clock::new().elapsed())
    }

    fn recruit(mut self, _ctx: tarpc::context::Context, partner: Partner) -> Self::RecruitFut {
        match self.partner {
            None => {
                self.partner = Some(partner);
                future::ok(self.oneself.into())
            }
            Some(partner) => future::err(ServiceError::HasAlreadyPartner(partner)),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
struct State {
    identity: Uuid,
    epoch: Duration,
}

impl State {
    fn new() -> Self {
        State {
            identity: Uuid::new_v4(),
            epoch: Clock::new().elapsed(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn error::Error>> {
    env_logger::init();

    let addrs_args: Vec<SocketAddr> = env::args()
        .skip(1)
        .map(|a| FromStr::from_str(a.as_str()).unwrap())
        .collect();
    let srv_addr = addrs_args[0];
    let peers_addrs: Vec<SocketAddr> = addrs_args[1..].to_vec();

    // XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX

    let st = match load_state(path::Path::new("state.cbor")) {
        Ok(st) => st,
        Err(_) => {
            let new_st = State::new();
            store_state(path::Path::new("state.cbor"), new_st)?;
            new_st
        }
    };
    let oneself = Oneself::new(st, srv_addr, peers_addrs.clone());

    for connect_peer in peers_addrs {
        let oneself_client = oneself.clone();
        tokio::spawn(async move {
            loop {
                {
                    log::info!("Trying to connect to peer: {}", connect_peer);
                    let r = client(oneself_client.clone(), connect_peer).await;
                    match r {
                        Ok(()) => (),
                        Err(err) => log::error!("Failed to connect to peer: {}", connect_peer),
                    }
                }

                tokio::time::sleep(Duration::SECOND).await;
            }
        });
    }

    log::info!("State: {:?}", st);
    log::info!("Running server: {}", srv_addr);

    serve(oneself.clone()).await?;

    // XXX
    //
    let sock = UdpSocket::bind("224.0.0.1:9090")?;

    let mut buf: Vec<u8> = vec::Vec::with_capacity(1024);

    loop {
        let before = time::Instant::now();
        thread::sleep(time::Duration::SECOND * 1);
        println!("Elapsed: {:?}", before.elapsed());

        buf.resize(1024, 0);
        let s = sock.recv(buf.as_mut_slice()).unwrap();
        buf.truncate(s);
        println!("Received: {:?}", String::from_utf8(buf.clone()).unwrap());
    }
}

fn load_state(path: &path::Path) -> Result<State, Box<dyn error::Error>> {
    let f = fs::File::open(path)?;
    match ciborium::de::from_reader(f) {
        Ok(v) => Ok(v),
        Err(err) => Err(Box::new(err)),
    }
}

fn store_state(path: &path::Path, state: State) -> Result<(), Box<dyn error::Error>> {
    let f = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    ciborium::ser::into_writer(&state, f)?;
    Ok(())
}

async fn client(oneself: Oneself, connect_peer: SocketAddr) -> Result<(), Box<dyn error::Error>> {
    let transport = tarpc::serde_transport::tcp::connect(
        connect_peer,
        tarpc::tokio_serde::formats::Bincode::default,
    );
    let client = ServiceClient::new(tarpc::client::Config::default(), transport.await?).spawn();

    let epoch = client.epoch(tarpc::context::current()).await?;
    let identity = client.identity(tarpc::context::current()).await?;
    println!("identity={}, epoch={:?}", identity, epoch);

    let recruit = client
        .recruit(tarpc::context::current(), oneself.into())
        .await?;
    println!("recruit = {:?}", recruit);

    loop {
        let now = client.now(tarpc::context::current()).await?;
        log::info!("now = {:?}", now);
    }
}

async fn serve(oneself: Oneself) -> Result<(), Box<dyn error::Error>> {
    let mut listener = tarpc::serde_transport::tcp::listen(
        oneself.addr,
        tarpc::tokio_serde::formats::Bincode::default,
    )
    .await?;

    listener.config_mut().max_frame_length(2048);
    listener
        .filter_map(|r| future::ready(r.ok()))
        .map(tarpc::server::BaseChannel::with_defaults)
        .map(|channel| {
            println!("peer={:?}", channel.transport().peer_addr());
            let server = Server::new(oneself.clone());
            channel.execute(server.serve())
        })
        .buffer_unordered(10)
        .for_each(|_| async {})
        .await;

    Ok(())
}
