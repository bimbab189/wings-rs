use chrono::{DateTime, Utc};
use serde::Serialize;
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Arc,
};
use tokio::sync::RwLock;
use utoipa::ToSchema;
use uuid::Uuid;

/// Unique identifier for a tracked SSH session.
pub type SessionId = Uuid;

/// The type of SSH session (shell, exec, or SFTP).
#[derive(Debug, Clone, Copy, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionType {
    Shell,
    Exec,
    Sftp,
}

/// Represents a single live SSH session.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SshSessionEntry {
    #[schema(value_type = String, format = Uuid)]
    pub id: SessionId,
    #[schema(value_type = String, format = Uuid)]
    pub server_uuid: Uuid,
    #[schema(value_type = String, format = Uuid)]
    pub user_uuid: Uuid,
    #[schema(value_type = String)]
    pub ip: IpAddr,
    pub session_type: SessionType,
    pub connected_since: DateTime<Utc>,
}

/// Thread-safe, in-memory registry of all live SSH sessions.
#[derive(Debug, Clone)]
pub struct SshSessionRegistry {
    sessions: Arc<RwLock<HashMap<SessionId, SshSessionEntry>>>,
}

impl Default for SshSessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SshSessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a new SSH session. Returns the session ID.
    pub async fn register(
        &self,
        server_uuid: Uuid,
        user_uuid: Uuid,
        ip: IpAddr,
        session_type: SessionType,
    ) -> SessionId {
        let id = Uuid::new_v4();
        let entry = SshSessionEntry {
            id,
            server_uuid,
            user_uuid,
            ip,
            session_type,
            connected_since: Utc::now(),
        };

        self.sessions.write().await.insert(id, entry);
        tracing::debug!(%id, %server_uuid, %user_uuid, ?session_type, "ssh session registered");

        id
    }

    /// Remove a session from the registry.
    pub async fn deregister(&self, session_id: SessionId) {
        if let Some(entry) = self.sessions.write().await.remove(&session_id) {
            tracing::debug!(
                id = %session_id,
                server = %entry.server_uuid,
                user = %entry.user_uuid,
                "ssh session deregistered"
            );
        }
    }

    /// Get all live sessions.
    pub async fn all(&self) -> Vec<SshSessionEntry> {
        self.sessions.read().await.values().cloned().collect()
    }

    /// Get all live sessions for a specific server.
    pub async fn for_server(&self, server_uuid: Uuid) -> Vec<SshSessionEntry> {
        self.sessions
            .read()
            .await
            .values()
            .filter(|s| s.server_uuid == server_uuid)
            .cloned()
            .collect()
    }

    /// Get the count of live sessions.
    pub async fn count(&self) -> usize {
        self.sessions.read().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[tokio::test]
    async fn test_register_and_deregister() {
        let registry = SshSessionRegistry::new();
        let server = Uuid::new_v4();
        let user = Uuid::new_v4();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);

        let id = registry.register(server, user, ip, SessionType::Shell).await;
        assert_eq!(registry.count().await, 1);

        let sessions = registry.for_server(server).await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].user_uuid, user);
        assert_eq!(sessions[0].session_type, SessionType::Shell);

        registry.deregister(id).await;
        assert_eq!(registry.count().await, 0);
    }

    #[tokio::test]
    async fn test_multiple_sessions() {
        let registry = SshSessionRegistry::new();
        let server1 = Uuid::new_v4();
        let server2 = Uuid::new_v4();
        let user = Uuid::new_v4();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);

        let _id1 = registry.register(server1, user, ip, SessionType::Shell).await;
        let _id2 = registry.register(server1, user, ip, SessionType::Sftp).await;
        let _id3 = registry.register(server2, user, ip, SessionType::Exec).await;

        assert_eq!(registry.count().await, 3);
        assert_eq!(registry.for_server(server1).await.len(), 2);
        assert_eq!(registry.for_server(server2).await.len(), 1);
        assert_eq!(registry.all().await.len(), 3);
    }

    #[tokio::test]
    async fn test_deregister_nonexistent() {
        let registry = SshSessionRegistry::new();
        registry.deregister(Uuid::new_v4()).await;
        assert_eq!(registry.count().await, 0);
    }
}
