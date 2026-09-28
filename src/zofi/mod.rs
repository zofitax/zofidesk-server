//! ZofiDesk additions to hbbs: user accounts, the client API and access control.

pub mod access;
pub mod api;
pub mod store;

use crate::common::get_arg_or;
use hbb_common::{log, tokio, ResultType};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

pub fn db_path() -> String {
    get_arg_or("ZOFI_DB", store::DEFAULT_PATH.to_owned())
}

/// Starts the ZofiDesk parts of hbbs:
/// - the API, on the port below the NAT test port (21114 by default); `API_PORT=0` disables it;
/// - with `MUST_LOGIN=Y`, access control: only signed-in users can connect or use the relay;
/// - with `EMBEDDED_RELAY=Y`, the relay inside this process (port 21117 by default), which is
///   needed for the relay to follow access control. The standalone hbbr never checks logins.
pub async fn start(bind_addr: Option<IpAddr>, rendezvous_port: i32, key: &str) -> ResultType<()> {
    let must_login = is_yes("MUST_LOGIN");
    let embedded_relay = is_yes("EMBEDDED_RELAY");
    log::info!("MUST_LOGIN={}", if must_login { "Y" } else { "N" });
    let port: u16 = get_arg_or("API_PORT", (rendezvous_port - 2).to_string())
        .parse()
        .unwrap_or(0);
    if must_login || port != 0 {
        match store::Db::open(&db_path()).await {
            Ok(db) => {
                if must_login {
                    access::enable(db.clone());
                    if !embedded_relay {
                        log::warn!(
                            "MUST_LOGIN=Y without EMBEDDED_RELAY=Y: an external relay does not check logins"
                        );
                    }
                }
                spawn_api(bind_addr, port, db);
            }
            // Without the database nobody could sign in, so refuse to run with MUST_LOGIN.
            Err(err) if must_login => return Err(err),
            Err(err) => log::error!("ZofiDesk API disabled, cannot open the database: {err}"),
        }
    }
    if embedded_relay {
        spawn_relay(bind_addr, rendezvous_port + 1, key);
    }
    Ok(())
}

fn is_yes(name: &str) -> bool {
    get_arg_or(name, String::new()).to_uppercase() == "Y"
}

/// `API_BIND` (e.g. 127.0.0.1 behind a reverse proxy) overrides the hbbs bind address for the API.
fn spawn_api(bind_addr: Option<IpAddr>, port: u16, db: store::Db) {
    if port == 0 {
        log::info!("ZofiDesk API disabled");
        return;
    }
    let api_bind = match crate::common::parse_bind_address(&get_arg_or("API_BIND", String::new())) {
        Ok(api_bind) => api_bind.or(bind_addr),
        Err(err) => {
            log::error!("ZofiDesk API disabled: {err}");
            return;
        }
    };
    let addr = SocketAddr::new(api_bind.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)), port);
    tokio::spawn(async move {
        if let Err(err) = api::serve(addr, db).await {
            log::error!("ZofiDesk API stopped: {err}");
        }
    });
}

// The relay builds its own runtime, so it gets its own thread; it shares access control
// through `access`'s process-wide state.
fn spawn_relay(bind_addr: Option<IpAddr>, port: i32, key: &str) {
    let key = key.to_owned();
    std::thread::spawn(move || {
        log::info!("Starting the embedded relay on port {port}");
        // It only returns Ok on a termination signal, which stops hbbs as well.
        if let Err(err) = crate::relay_server::start_with_bind(bind_addr, &port.to_string(), &key) {
            log::error!("Embedded relay stopped: {err}");
            // Running on without the relay would silently break relayed connections.
            std::process::exit(1);
        }
    });
}
