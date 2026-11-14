// SPDX-License-Identifier: GPL-3.0-or-later

//! The seat0 session as systemd-logind sees it: who is in front of the
//! screen, and whether it is locked.

use std::time::Duration;

use zbus::Connection;
use zbus::zvariant::OwnedObjectPath;

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    gen_blocking = false
)]
trait Manager {
    fn lock_session(&self, session_id: &str) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Seat",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1/seat/seat0",
    gen_blocking = false
)]
trait Seat {
    #[zbus(property)]
    fn active_session(&self) -> zbus::Result<(String, OwnedObjectPath)>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    gen_blocking = false
)]
trait Session {
    #[zbus(property)]
    fn user(&self) -> zbus::Result<(u32, OwnedObjectPath)>;
    #[zbus(property)]
    fn class(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn locked_hint(&self) -> zbus::Result<bool>;
}

/// The active session of seat0.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Active {
    /// logind session id, empty when no session is active.
    pub id: String,
    pub uid: Option<u32>,
    /// Locked, on the greeter, or no session at all: the user's data is
    /// not given out.
    pub locked: bool,
}

pub struct Logind {
    conn: Connection,
}

impl Logind {
    pub async fn connect() -> zbus::Result<Self> {
        Ok(Self {
            conn: Connection::system().await?,
        })
    }

    pub async fn active(&self) -> zbus::Result<Active> {
        let seat = SeatProxy::builder(&self.conn)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        let (id, path) = seat.active_session().await?;
        if id.is_empty() || path.as_str() == "/" {
            return Ok(Active {
                locked: true,
                ..Active::default()
            });
        }
        let session = SessionProxy::builder(&self.conn)
            .path(path)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        let (uid, _) = session.user().await?;
        let class = session.class().await.unwrap_or_default();
        let locked_hint = session.locked_hint().await.unwrap_or(false);
        Ok(Active {
            id,
            uid: Some(uid),
            locked: locked_hint || class == "greeter" || class == "lock-screen",
        })
    }

    pub async fn lock(&self, session_id: &str) -> zbus::Result<()> {
        ManagerProxy::new(&self.conn)
            .await?
            .lock_session(session_id)
            .await
    }
}

/// Polls logind and reports each change of the active session.
pub fn watch(
    logind: std::sync::Arc<Logind>,
    changed: tokio::sync::mpsc::UnboundedSender<Active>,
) {
    tokio::spawn(async move {
        let mut last: Option<Active> = None;
        let mut warned = false;
        loop {
            match logind.active().await {
                Ok(now) => {
                    warned = false;
                    if last.as_ref() != Some(&now) {
                        tracing::info!(session = now.id, uid = ?now.uid, locked = now.locked, "seat0 active session");
                        if changed.send(now.clone()).is_err() {
                            return;
                        }
                        last = Some(now);
                    }
                }
                Err(e) if !warned => {
                    warned = true;
                    tracing::warn!("cannot read the seat0 session from logind: {e}");
                }
                Err(_) => {}
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
}
