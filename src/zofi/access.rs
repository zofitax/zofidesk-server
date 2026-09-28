//! Access control for connections (`MUST_LOGIN=Y`): only a signed-in ZofiDesk user can start a
//! connection or use the relay. Controlled devices still register and accept connections without
//! an account.
//!
//! The controlling client sends its login token to hbbs in `PunchHoleRequest` and in
//! `RequestRelay`, but not to the relay, which only sees a uuid chosen by one of the two peers.
//! So hbbs authorizes those uuids here and the relay (embedded in hbbs, so both share this state)
//! only pairs authorized ones:
//! - a uuid sent by a signed-in controller in `RequestRelay`;
//! - a uuid chosen by the controlled device in `RelayResponse`, when it answers a controller
//!   that was authorized for that device moments before.
//!
//! When `MUST_LOGIN` is off nothing is checked and every hook allows everything.

use super::store::Db;
use hbb_common::{log, try_into_v4};
use once_cell::sync::OnceCell;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

// Logouts through the API take effect at once; disabling a user or changing a password with
// zofidesk-admin (another process) takes effect within this time.
const TOKEN_CACHE_TTL: Duration = Duration::from_secs(30);
// Long enough for the client's three punch attempts and the device's relay answer.
const CONTROLLER_TTL: Duration = Duration::from_secs(60);
// Both peers must reach the relay within this time; the relay itself waits 30s for the second.
const RELAY_UUID_TTL: Duration = Duration::from_secs(60);
// Expired entries are only swept once a map grows past this size.
const PRUNE_THRESHOLD: usize = 1024;

const NOT_SIGNED_IN: &str = "Debe iniciar sesión en ZofiDesk para conectarse a otros equipos";
const SESSION_EXPIRED: &str = "Su sesión de ZofiDesk ha caducado. Vuelva a iniciar sesión";
const INTERNAL_ERROR: &str = "Error interno del servidor. Inténtelo de nuevo";

static ACCESS: OnceCell<Access> = OnceCell::new();

/// Turns access control on for the rest of the process.
pub fn enable(db: Db) {
    if ACCESS.set(Access::new(db)).is_err() {
        log::warn!("ZofiDesk access control was already enabled");
    }
}

/// Checks a `PunchHoleRequest` from `addr` to device `id`. Returns the refusal message, if any.
pub async fn check_punch_hole(addr: SocketAddr, token: &str, id: &str) -> Option<String> {
    match ACCESS.get() {
        Some(access) => access.check_controller(addr, token, id).await.err(),
        None => None,
    }
}

/// Checks a `RequestRelay` sent to hbbs and, when allowed, authorizes its uuid on the relay.
pub async fn check_request_relay(
    addr: SocketAddr,
    token: &str,
    id: &str,
    uuid: &str,
) -> Option<String> {
    let access = ACCESS.get()?;
    if let Err(reason) = access.check_controller(addr, token, id).await {
        return Some(reason);
    }
    access.allow_relay_uuid(uuid);
    None
}

/// A device's `RelayResponse` for the controller at `controller_addr`: authorizes the uuid the
/// device chose when that controller was just authorized to connect to device `id`.
pub fn relay_response(controller_addr: SocketAddr, id: &str, uuid: &str) {
    if let Some(access) = ACCESS.get() {
        access.relay_response(controller_addr, id, uuid);
    }
}

/// Whether the relay may pair connections with this uuid.
pub fn relay_allowed(uuid: &str) -> bool {
    match ACCESS.get() {
        Some(access) => access.relay_allowed(uuid),
        None => true,
    }
}

/// Drops a token from the cache after a logout.
pub fn forget_token(token: &str) {
    if let Some(access) = ACCESS.get() {
        access.tokens.lock().unwrap().remove(token);
    }
}

struct Access {
    db: Db,
    // token -> (username if the token is valid, when it was checked)
    tokens: Mutex<HashMap<String, (Option<String>, Instant)>>,
    // controller address -> (device it was authorized to reach, when)
    controllers: Mutex<HashMap<SocketAddr, (String, Instant)>>,
    // relay uuid -> when it was authorized
    relay_uuids: Mutex<HashMap<String, Instant>>,
}

impl Access {
    fn new(db: Db) -> Self {
        Access {
            db,
            tokens: Default::default(),
            controllers: Default::default(),
            relay_uuids: Default::default(),
        }
    }

    async fn check_controller(&self, addr: SocketAddr, token: &str, id: &str) -> Result<(), String> {
        if token.is_empty() {
            log::info!("Refused connection from {addr} to {id}: not signed in");
            return Err(NOT_SIGNED_IN.to_owned());
        }
        match self.user_for_token(token).await? {
            Some(username) => {
                log::debug!("User {username} connecting from {addr} to {id}");
                let mut controllers = self.controllers.lock().unwrap();
                prune(&mut controllers, CONTROLLER_TTL, |(_, tm)| *tm);
                controllers.insert(try_into_v4(addr), (id.to_owned(), Instant::now()));
                Ok(())
            }
            None => {
                log::info!("Refused connection from {addr} to {id}: invalid or expired session");
                Err(SESSION_EXPIRED.to_owned())
            }
        }
    }

    async fn user_for_token(&self, token: &str) -> Result<Option<String>, String> {
        if let Some((user, tm)) = self.tokens.lock().unwrap().get(token) {
            if tm.elapsed() < TOKEN_CACHE_TTL {
                return Ok(user.clone());
            }
        }
        let user = match self.db.user_for_token(token).await {
            Ok(user) => user.map(|user| user.username),
            Err(err) => {
                log::error!("Failed to check a session token: {err}");
                return Err(INTERNAL_ERROR.to_owned());
            }
        };
        let mut tokens = self.tokens.lock().unwrap();
        prune(&mut tokens, TOKEN_CACHE_TTL, |(_, tm)| *tm);
        tokens.insert(token.to_owned(), (user.clone(), Instant::now()));
        Ok(user)
    }

    fn relay_response(&self, controller_addr: SocketAddr, id: &str, uuid: &str) {
        if uuid.is_empty() {
            return;
        }
        let authorized = matches!(
            self.controllers.lock().unwrap().get(&try_into_v4(controller_addr)),
            Some((target, tm)) if target == id && tm.elapsed() < CONTROLLER_TTL
        );
        if authorized {
            self.allow_relay_uuid(uuid);
        } else {
            log::info!("Relay {uuid} from {id} not authorized: no signed-in controller");
        }
    }

    fn allow_relay_uuid(&self, uuid: &str) {
        if uuid.is_empty() {
            return;
        }
        let mut relay_uuids = self.relay_uuids.lock().unwrap();
        prune(&mut relay_uuids, RELAY_UUID_TTL, |tm| *tm);
        relay_uuids.insert(uuid.to_owned(), Instant::now());
    }

    fn relay_allowed(&self, uuid: &str) -> bool {
        matches!(
            self.relay_uuids.lock().unwrap().get(uuid),
            Some(tm) if tm.elapsed() < RELAY_UUID_TTL
        )
    }
}

fn prune<K, V>(map: &mut HashMap<K, V>, ttl: Duration, time: impl Fn(&V) -> Instant) {
    if map.len() >= PRUNE_THRESHOLD {
        map.retain(|_, value| time(value).elapsed() < ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zofi::store::tests::temp_db;
    use hbb_common::tokio;

    const CONTROLLER: &str = "203.0.113.5:40000";

    async fn signed_in(access: &Access) -> String {
        access.db.add_user("ana", "password1", false).await.unwrap();
        let user = access.db.check_password("ana", "password1").await.unwrap().unwrap();
        access.db.create_token(user.id, "", "").await.unwrap()
    }

    #[tokio::test]
    async fn only_signed_in_users_can_connect() {
        let temp = temp_db().await;
        let access = Access::new(temp.db.clone());
        let addr = CONTROLLER.parse().unwrap();
        assert_eq!(access.check_controller(addr, "", "111").await, Err(NOT_SIGNED_IN.to_owned()));
        assert_eq!(
            access.check_controller(addr, "forged", "111").await,
            Err(SESSION_EXPIRED.to_owned())
        );
        let token = signed_in(&access).await;
        assert_eq!(access.check_controller(addr, &token, "111").await, Ok(()));
    }

    #[tokio::test]
    async fn revoked_session_is_refused_after_cache() {
        let temp = temp_db().await;
        let access = Access::new(temp.db.clone());
        let addr = CONTROLLER.parse().unwrap();
        let token = signed_in(&access).await;
        assert!(access.check_controller(addr, &token, "111").await.is_ok());
        temp.db.set_active("ana", false).await.unwrap();
        // Still cached...
        assert!(access.check_controller(addr, &token, "111").await.is_ok());
        // ...until the entry expires or the token is dropped on logout.
        access.tokens.lock().unwrap().remove(&token);
        assert!(access.check_controller(addr, &token, "111").await.is_err());
    }

    #[tokio::test]
    async fn relay_needs_an_authorized_uuid() {
        let temp = temp_db().await;
        let access = Access::new(temp.db.clone());
        let addr: SocketAddr = CONTROLLER.parse().unwrap();
        let token = signed_in(&access).await;
        assert!(!access.relay_allowed("chosen-by-anyone"));

        // Controller asks hbbs for a relay with its own uuid.
        access.check_controller(addr, &token, "111").await.unwrap();
        access.allow_relay_uuid("controller-uuid");
        assert!(access.relay_allowed("controller-uuid"));

        // The device answers a controller authorized for it: its uuid is allowed...
        access.relay_response(addr, "111", "device-uuid");
        assert!(access.relay_allowed("device-uuid"));
        // ...but not when it claims another device, or answers an unknown controller.
        access.relay_response(addr, "222", "other-device-uuid");
        assert!(!access.relay_allowed("other-device-uuid"));
        access.relay_response("198.51.100.7:5000".parse().unwrap(), "111", "stranger-uuid");
        assert!(!access.relay_allowed("stranger-uuid"));
    }

    #[tokio::test]
    async fn expired_entries_are_not_honoured() {
        let temp = temp_db().await;
        let access = Access::new(temp.db.clone());
        let old = Instant::now() - RELAY_UUID_TTL - Duration::from_secs(1);
        access.relay_uuids.lock().unwrap().insert("old".to_owned(), old);
        assert!(!access.relay_allowed("old"));
        let addr: SocketAddr = CONTROLLER.parse().unwrap();
        access.controllers.lock().unwrap().insert(addr, ("111".to_owned(), old));
        access.relay_response(addr, "111", "late-uuid");
        assert!(!access.relay_allowed("late-uuid"));
    }
}
