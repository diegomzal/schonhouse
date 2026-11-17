//! Persistent storage: an embedded SQLite database (via `rusqlite`, bundled).
//!
//! One file, WAL mode, foreign keys on. Devices, groups and scenes live here; server
//! settings stay in the TOML config. All queries run on the blocking thread pool so
//! the async runtime is never stalled, and every write is a single transaction.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::config::{DeviceConfig, DeviceKind};
use crate::devices::LightCommand;

const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Group {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    /// Device ids, in display order.
    pub devices: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SceneEntry {
    pub device_id: String,
    pub command: LightCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scene {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    pub entries: Vec<SceneEntry>,
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open database {}", path.display()))?;
        Self::init(conn)
    }

    /// In-memory database, used by tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )?;
        migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Runs a closure against the connection on the blocking pool.
    async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|_| anyhow!("database mutex poisoned"))?;
            f(&mut guard)
        })
        .await
        .context("database task panicked")?
    }

    // ------------------------------------------------------------------ devices

    pub async fn list_devices(&self) -> Result<Vec<DeviceConfig>> {
        self.run(|conn| {
            let mut stmt =
                conn.prepare("SELECT id, name, kind, ip FROM devices ORDER BY created_at, id")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            let mut devices = Vec::new();
            for row in rows {
                let (id, name, kind, ip) = row?;
                let kind = parse_kind(&kind)
                    .with_context(|| format!("device {id:?} has unknown kind {kind:?}"))?;
                devices.push(DeviceConfig { id, name, kind, ip });
            }
            Ok(devices)
        })
        .await
    }

    pub async fn insert_device(&self, device: DeviceConfig) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO devices (id, name, kind, ip, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    device.id,
                    device.name,
                    kind_name(device.kind),
                    device.ip,
                    unix_now()
                ],
            )
            .map_err(|err| match err {
                rusqlite::Error::SqliteFailure(e, _)
                    if e.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    anyhow!("a device with the same id or ip already exists")
                }
                other => other.into(),
            })?;
            Ok(())
        })
        .await
    }

    /// Returns `false` when the device does not exist.
    pub async fn update_device_name(&self, id: String, name: Option<String>) -> Result<bool> {
        self.run(move |conn| {
            let n = conn.execute(
                "UPDATE devices SET name = ?2 WHERE id = ?1",
                params![id, name],
            )?;
            Ok(n > 0)
        })
        .await
    }

    pub async fn delete_device(&self, id: String) -> Result<bool> {
        self.run(move |conn| Ok(conn.execute("DELETE FROM devices WHERE id = ?1", [id])? > 0))
            .await
    }

    // ------------------------------------------------------------------ groups

    pub async fn list_groups(&self) -> Result<Vec<Group>> {
        self.run(|conn| {
            let mut stmt = conn.prepare("SELECT id, name FROM groups ORDER BY created_at, id")?;
            let groups: Vec<(i64, String)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()?;
            let mut members = conn.prepare(
                "SELECT device_id FROM group_members WHERE group_id = ?1 ORDER BY position",
            )?;
            let mut out = Vec::with_capacity(groups.len());
            for (id, name) in groups {
                let devices = members
                    .query_map([id], |row| row.get(0))?
                    .collect::<Result<Vec<String>, _>>()?;
                out.push(Group { id, name, devices });
            }
            Ok(out)
        })
        .await
    }

    pub async fn get_group(&self, id: i64) -> Result<Option<Group>> {
        Ok(self.list_groups().await?.into_iter().find(|g| g.id == id))
    }

    /// Inserts (id == 0) or replaces a group with its members. Returns the stored group.
    pub async fn save_group(&self, group: Group) -> Result<Group> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let id = if group.id == 0 {
                tx.execute(
                    "INSERT INTO groups (name, created_at) VALUES (?1, ?2)",
                    params![group.name, unix_now()],
                )?;
                tx.last_insert_rowid()
            } else {
                let n = tx.execute(
                    "UPDATE groups SET name = ?2 WHERE id = ?1",
                    params![group.id, group.name],
                )?;
                if n == 0 {
                    return Err(anyhow!("group {} does not exist", group.id));
                }
                tx.execute("DELETE FROM group_members WHERE group_id = ?1", [group.id])?;
                group.id
            };
            for (position, device_id) in group.devices.iter().enumerate() {
                tx.execute(
                    "INSERT INTO group_members (group_id, device_id, position) VALUES (?1, ?2, ?3)",
                    params![id, device_id, position as i64],
                )
                .map_err(|_| anyhow!("unknown device {device_id:?}"))?;
            }
            tx.commit()?;
            Ok(Group { id, ..group })
        })
        .await
    }

    pub async fn delete_group(&self, id: i64) -> Result<bool> {
        self.run(move |conn| Ok(conn.execute("DELETE FROM groups WHERE id = ?1", [id])? > 0))
            .await
    }

    // ------------------------------------------------------------------ scenes

    pub async fn list_scenes(&self) -> Result<Vec<Scene>> {
        self.run(|conn| {
            let mut stmt = conn.prepare("SELECT id, name FROM scenes ORDER BY created_at, id")?;
            let scenes: Vec<(i64, String)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()?;
            let mut entries = conn.prepare(
                "SELECT device_id, command FROM scene_entries WHERE scene_id = ?1 ORDER BY rowid",
            )?;
            let mut out = Vec::with_capacity(scenes.len());
            for (id, name) in scenes {
                let rows = entries.query_map([id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                let mut list = Vec::new();
                for row in rows {
                    let (device_id, command) = row?;
                    let command: LightCommand = serde_json::from_str(&command)
                        .with_context(|| format!("scene {id} has an invalid entry"))?;
                    list.push(SceneEntry { device_id, command });
                }
                out.push(Scene {
                    id,
                    name,
                    entries: list,
                });
            }
            Ok(out)
        })
        .await
    }

    pub async fn get_scene(&self, id: i64) -> Result<Option<Scene>> {
        Ok(self.list_scenes().await?.into_iter().find(|s| s.id == id))
    }

    /// Inserts (id == 0) or replaces a scene with its entries. Returns the stored scene.
    pub async fn save_scene(&self, scene: Scene) -> Result<Scene> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let id = if scene.id == 0 {
                tx.execute(
                    "INSERT INTO scenes (name, created_at) VALUES (?1, ?2)",
                    params![scene.name, unix_now()],
                )?;
                tx.last_insert_rowid()
            } else {
                let n = tx.execute(
                    "UPDATE scenes SET name = ?2 WHERE id = ?1",
                    params![scene.id, scene.name],
                )?;
                if n == 0 {
                    return Err(anyhow!("scene {} does not exist", scene.id));
                }
                tx.execute("DELETE FROM scene_entries WHERE scene_id = ?1", [scene.id])?;
                scene.id
            };
            for entry in &scene.entries {
                let command = serde_json::to_string(&entry.command)?;
                tx.execute(
                    "INSERT INTO scene_entries (scene_id, device_id, command) VALUES (?1, ?2, ?3)",
                    params![id, entry.device_id, command],
                )
                .map_err(|_| anyhow!("unknown or duplicate device {:?}", entry.device_id))?;
            }
            tx.commit()?;
            Ok(Scene { id, ..scene })
        })
        .await
    }

    pub async fn delete_scene(&self, id: i64) -> Result<bool> {
        self.run(move |conn| Ok(conn.execute("DELETE FROM scenes WHERE id = ?1", [id])? > 0))
            .await
    }

    #[cfg(test)]
    pub async fn schema_version(&self) -> Result<i64> {
        self.run(|conn| Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?))
            .await
    }
}

fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(anyhow!(
            "database schema version {version} is newer than this binary supports ({SCHEMA_VERSION})"
        ));
    }
    if version < 1 {
        conn.execute_batch(
            "CREATE TABLE devices (
                 id         TEXT PRIMARY KEY,
                 name       TEXT,
                 kind       TEXT NOT NULL,
                 ip         TEXT NOT NULL UNIQUE,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE groups (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 name       TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE group_members (
                 group_id  INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
                 device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
                 position  INTEGER NOT NULL,
                 PRIMARY KEY (group_id, device_id)
             );
             CREATE TABLE scenes (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 name       TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE scene_entries (
                 scene_id  INTEGER NOT NULL REFERENCES scenes(id) ON DELETE CASCADE,
                 device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
                 command   TEXT NOT NULL,
                 PRIMARY KEY (scene_id, device_id)
             );
             PRAGMA user_version = 1;",
        )?;
    }
    Ok(())
}

fn kind_name(kind: DeviceKind) -> &'static str {
    match kind {
        DeviceKind::TapoL535 => "tapo-l535",
    }
}

fn parse_kind(name: &str) -> Option<DeviceKind> {
    match name {
        "tapo-l535" => Some(DeviceKind::TapoL535),
        _ => None,
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, ip: &str) -> DeviceConfig {
        DeviceConfig {
            id: id.into(),
            name: None,
            kind: DeviceKind::TapoL535,
            ip: ip.into(),
        }
    }

    #[tokio::test]
    async fn devices_round_trip() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.schema_version().await.unwrap(), SCHEMA_VERSION);
        store.insert_device(device("a", "10.0.0.1")).await.unwrap();
        store.insert_device(device("b", "10.0.0.2")).await.unwrap();
        assert!(store.insert_device(device("a", "10.0.0.9")).await.is_err());
        assert!(store.insert_device(device("c", "10.0.0.1")).await.is_err());

        let list = store.list_devices().await.unwrap();
        assert_eq!(
            list.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );

        assert!(
            store
                .update_device_name("a".into(), Some("Desk".into()))
                .await
                .unwrap()
        );
        assert!(!store.update_device_name("zz".into(), None).await.unwrap());
        assert_eq!(
            store.list_devices().await.unwrap()[0].name.as_deref(),
            Some("Desk")
        );

        assert!(store.delete_device("a".into()).await.unwrap());
        assert!(!store.delete_device("a".into()).await.unwrap());
        assert_eq!(store.list_devices().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn groups_and_scenes_cascade() {
        let store = Store::open_in_memory().unwrap();
        store.insert_device(device("a", "10.0.0.1")).await.unwrap();
        store.insert_device(device("b", "10.0.0.2")).await.unwrap();

        let group = store
            .save_group(Group {
                id: 0,
                name: "All".into(),
                devices: vec!["b".into(), "a".into()],
            })
            .await
            .unwrap();
        assert!(group.id > 0);
        assert_eq!(store.list_groups().await.unwrap()[0].devices, ["b", "a"]);

        // Unknown member is rejected and the transaction rolled back.
        let bad = store
            .save_group(Group {
                id: group.id,
                name: "All".into(),
                devices: vec!["nope".into()],
            })
            .await;
        assert!(bad.is_err());
        assert_eq!(
            store.get_group(group.id).await.unwrap().unwrap().devices,
            ["b", "a"]
        );

        let scene = store
            .save_scene(Scene {
                id: 0,
                name: "Movie".into(),
                entries: vec![SceneEntry {
                    device_id: "a".into(),
                    command: LightCommand {
                        on: Some(true),
                        brightness: Some(20),
                        ..Default::default()
                    },
                }],
            })
            .await
            .unwrap();
        assert_eq!(
            store.get_scene(scene.id).await.unwrap().unwrap().entries[0]
                .command
                .brightness,
            Some(20)
        );

        // Deleting a device removes it from groups and scenes.
        store.delete_device("a".into()).await.unwrap();
        assert_eq!(
            store.get_group(group.id).await.unwrap().unwrap().devices,
            ["b"]
        );
        assert!(
            store
                .get_scene(scene.id)
                .await
                .unwrap()
                .unwrap()
                .entries
                .is_empty()
        );

        assert!(store.delete_scene(scene.id).await.unwrap());
        assert!(store.delete_group(group.id).await.unwrap());
        assert!(store.list_scenes().await.unwrap().is_empty());
    }
}
