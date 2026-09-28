//! ZofiDesk additions to hbbs: user accounts and the client API.

pub mod api;
pub mod db;

use crate::common::get_arg_or;
use hbb_common::{log, tokio};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

pub fn db_path() -> String {
    get_arg_or("ZOFI_DB", db::DEFAULT_PATH.to_owned())
}

/// Starts the API next to hbbs, on the port below the NAT test port (21114 by default).
/// `API_PORT=0` disables it.
pub fn spawn_api(bind_addr: Option<IpAddr>, rendezvous_port: i32) {
    let port: u16 = get_arg_or("API_PORT", (rendezvous_port - 2).to_string())
        .parse()
        .unwrap_or(0);
    if port == 0 {
        log::info!("ZofiDesk API disabled");
        return;
    }
    let addr = SocketAddr::new(bind_addr.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)), port);
    tokio::spawn(async move {
        let result = match db::Db::open(&db_path()).await {
            Ok(db) => api::serve(addr, db).await,
            Err(err) => Err(err),
        };
        if let Err(err) = result {
            log::error!("ZofiDesk API stopped: {err}");
        }
    });
}
