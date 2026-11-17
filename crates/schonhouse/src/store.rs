//! Persistent storage: an embedded SQLite database (via `rusqlite`, bundled).
//!
//! One file, WAL mode, foreign keys on. Devices, rooms, groups, scenes, presets and
//! usage samples live here; server settings stay in the TOML config. All queries run
//! on the blocking thread pool so the async runtime is never stalled, and every write
//! is a single transaction. `PRAGMA user_version` tracks the schema; `migrate` applies
//! the steps from the stored version up to `SCHEMA_VERSION`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::config::{DeviceConfig, DeviceKind};
use crate::devices::LightCommand;

const SCHEMA_VERSION: i64 = 2;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Room {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub position: i64,
}

/// Where a device sits in the UI: its room (if any) and its order within it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceLayout {
    pub id: String,
    pub room_id: Option<i64>,
    #[serde(default)]
    pub position: i64,
}

/// A saved light setting (color or white) that can be applied to any device or group.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Preset {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    pub command: LightCommand,
    #[serde(default)]
    pub position: i64,
}

/// One reading of a device's usage counters. Times are minutes, energy is Wh, as
/// reported by the device for the current day and the trailing 7 and 30 days.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageSample {
    pub device_id: String,
    pub ts: i64,
    pub time_today: Option<i64>,
    pub time_past7: Option<i64>,
    pub time_past30: Option<i64>,
    pub power_today: Option<i64>,
    pub power_past7: Option<i64>,
    pub power_past30: Option<i64>,
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
            let mut stmt = conn.prepare(
                "SELECT id, name, kind, ip FROM devices ORDER BY position, created_at, id",
            )?;
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
            let position: i64 = conn.query_row(
                "SELECT COALESCE(MAX(position), -1) + 1 FROM devices",
                [],
                |row| row.get(0),
            )?;
            conn.execute(
                "INSERT INTO devices (id, name, kind, ip, created_at, position)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    device.id,
                    device.name,
                    kind_name(device.kind),
                    device.ip,
                    unix_now(),
                    position
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

    // ------------------------------------------------------------------ rooms and layout

    pub async fn list_rooms(&self) -> Result<Vec<Room>> {
        self.run(|conn| {
            let mut stmt =
                conn.prepare("SELECT id, name, position FROM rooms ORDER BY position, id")?;
            let rooms = stmt
                .query_map([], |row| {
                    Ok(Room {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        position: row.get(2)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rooms)
        })
        .await
    }

    /// Inserts (id == 0) or updates a room. Returns the stored room.
    pub async fn save_room(&self, room: Room) -> Result<Room> {
        self.run(move |conn| {
            if room.id == 0 {
                let position: i64 = conn.query_row(
                    "SELECT COALESCE(MAX(position), -1) + 1 FROM rooms",
                    [],
                    |row| row.get(0),
                )?;
                conn.execute(
                    "INSERT INTO rooms (name, position, created_at) VALUES (?1, ?2, ?3)",
                    params![room.name, position, unix_now()],
                )?;
                Ok(Room {
                    id: conn.last_insert_rowid(),
                    position,
                    ..room
                })
            } else {
                let n = conn.execute(
                    "UPDATE rooms SET name = ?2, position = ?3 WHERE id = ?1",
                    params![room.id, room.name, room.position],
                )?;
                if n == 0 {
                    return Err(anyhow!("room {} does not exist", room.id));
                }
                Ok(room)
            }
        })
        .await
    }

    /// Deleting a room leaves its devices unassigned.
    pub async fn delete_room(&self, id: i64) -> Result<bool> {
        self.run(move |conn| Ok(conn.execute("DELETE FROM rooms WHERE id = ?1", [id])? > 0))
            .await
    }

    pub async fn list_layout(&self) -> Result<Vec<DeviceLayout>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, room_id, position FROM devices ORDER BY position, created_at, id",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(DeviceLayout {
                        id: row.get(0)?,
                        room_id: row.get(1)?,
                        position: row.get(2)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Applies room and position to each listed device in one transaction. Unknown
    /// rooms are rejected; unknown devices are ignored. Returns how many were updated.
    pub async fn set_layout(&self, layout: Vec<DeviceLayout>) -> Result<usize> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let mut updated = 0;
            for item in &layout {
                updated += tx
                    .execute(
                        "UPDATE devices SET room_id = ?2, position = ?3 WHERE id = ?1",
                        params![item.id, item.room_id, item.position],
                    )
                    .map_err(|_| anyhow!("unknown room for {:?}", item.id))?;
            }
            tx.commit()?;
            Ok(updated)
        })
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

    // ------------------------------------------------------------------ presets

    pub async fn list_presets(&self) -> Result<Vec<Preset>> {
        self.run(|conn| {
            let mut stmt = conn
                .prepare("SELECT id, name, command, position FROM presets ORDER BY position, id")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, name, command, position) = row?;
                let command = serde_json::from_str(&command)
                    .with_context(|| format!("preset {id} has an invalid command"))?;
                out.push(Preset {
                    id,
                    name,
                    command,
                    position,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Inserts (id == 0) or updates a preset. Returns the stored preset.
    pub async fn save_preset(&self, preset: Preset) -> Result<Preset> {
        self.run(move |conn| {
            let command = serde_json::to_string(&preset.command)?;
            if preset.id == 0 {
                let position: i64 = conn.query_row(
                    "SELECT COALESCE(MAX(position), -1) + 1 FROM presets",
                    [],
                    |row| row.get(0),
                )?;
                conn.execute(
                    "INSERT INTO presets (name, command, position, created_at) VALUES (?1, ?2, ?3, ?4)",
                    params![preset.name, command, position, unix_now()],
                )?;
                Ok(Preset {
                    id: conn.last_insert_rowid(),
                    position,
                    ..preset
                })
            } else {
                let n = conn.execute(
                    "UPDATE presets SET name = ?2, command = ?3, position = ?4 WHERE id = ?1",
                    params![preset.id, preset.name, command, preset.position],
                )?;
                if n == 0 {
                    return Err(anyhow!("preset {} does not exist", preset.id));
                }
                Ok(preset)
            }
        })
        .await
    }

    pub async fn delete_preset(&self, id: i64) -> Result<bool> {
        self.run(move |conn| Ok(conn.execute("DELETE FROM presets WHERE id = ?1", [id])? > 0))
            .await
    }

    // ------------------------------------------------------------------ usage samples

    pub async fn insert_usage_sample(&self, sample: UsageSample) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO usage_samples
                 (device_id, ts, time_today, time_past7, time_past30, power_today, power_past7, power_past30)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    sample.device_id,
                    sample.ts,
                    sample.time_today,
                    sample.time_past7,
                    sample.time_past30,
                    sample.power_today,
                    sample.power_past7,
                    sample.power_past30
                ],
            )
            .map_err(|_| anyhow!("unknown device {:?}", sample.device_id))?;
            Ok(())
        })
        .await
    }

    /// Samples since `since` (unix seconds), optionally for one device, oldest first.
    pub async fn list_usage(
        &self,
        device_id: Option<String>,
        since: i64,
    ) -> Result<Vec<UsageSample>> {
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT device_id, ts, time_today, time_past7, time_past30,
                        power_today, power_past7, power_past30
                 FROM usage_samples
                 WHERE ts >= ?1 AND (?2 IS NULL OR device_id = ?2)
                 ORDER BY ts, device_id",
            )?;
            let rows = stmt
                .query_map(params![since, device_id], row_to_sample)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// The most recent sample of every device.
    pub async fn latest_usage(&self) -> Result<Vec<UsageSample>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT u.device_id, u.ts, u.time_today, u.time_past7, u.time_past30,
                        u.power_today, u.power_past7, u.power_past30
                 FROM usage_samples u
                 JOIN (SELECT device_id, MAX(ts) AS ts FROM usage_samples GROUP BY device_id) m
                   ON m.device_id = u.device_id AND m.ts = u.ts
                 ORDER BY u.device_id",
            )?;
            let rows = stmt
                .query_map([], row_to_sample)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Drops samples older than `before` (unix seconds). Returns how many were removed.
    pub async fn prune_usage(&self, before: i64) -> Result<usize> {
        self.run(move |conn| Ok(conn.execute("DELETE FROM usage_samples WHERE ts < ?1", [before])?))
            .await
    }

    #[cfg(test)]
    pub async fn schema_version(&self) -> Result<i64> {
        self.run(|conn| Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?))
            .await
    }
}

fn row_to_sample(row: &rusqlite::Row<'_>) -> rusqlite::Result<UsageSample> {
    Ok(UsageSample {
        device_id: row.get(0)?,
        ts: row.get(1)?,
        time_today: row.get(2)?,
        time_past7: row.get(3)?,
        time_past30: row.get(4)?,
        power_today: row.get(5)?,
        power_past7: row.get(6)?,
        power_past30: row.get(7)?,
    })
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
    if version < 2 {
        conn.execute_batch(
            "CREATE TABLE rooms (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 name       TEXT NOT NULL,
                 position   INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL
             );
             ALTER TABLE devices ADD COLUMN room_id INTEGER REFERENCES rooms(id) ON DELETE SET NULL;
             ALTER TABLE devices ADD COLUMN position INTEGER NOT NULL DEFAULT 0;
             CREATE TABLE presets (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 name       TEXT NOT NULL,
                 command    TEXT NOT NULL,
                 position   INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE usage_samples (
                 device_id    TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
                 ts           INTEGER NOT NULL,
                 time_today   INTEGER,
                 time_past7   INTEGER,
                 time_past30  INTEGER,
                 power_today  INTEGER,
                 power_past7  INTEGER,
                 power_past30 INTEGER,
                 PRIMARY KEY (device_id, ts)
             );
             CREATE INDEX usage_samples_ts ON usage_samples (ts);
             PRAGMA user_version = 2;",
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

    #[tokio::test]
    async fn rooms_and_layout() {
        let store = Store::open_in_memory().unwrap();
        store.insert_device(device("a", "10.0.0.1")).await.unwrap();
        store.insert_device(device("b", "10.0.0.2")).await.unwrap();

        let living = store
            .save_room(Room {
                id: 0,
                name: "Living".into(),
                position: 0,
            })
            .await
            .unwrap();
        let bedroom = store
            .save_room(Room {
                id: 0,
                name: "Bedroom".into(),
                position: 0,
            })
            .await
            .unwrap();
        assert_eq!(bedroom.position, 1);

        let updated = store
            .set_layout(vec![
                DeviceLayout {
                    id: "a".into(),
                    room_id: Some(living.id),
                    position: 5,
                },
                DeviceLayout {
                    id: "b".into(),
                    room_id: Some(bedroom.id),
                    position: 1,
                },
                DeviceLayout {
                    id: "ghost".into(),
                    room_id: None,
                    position: 0,
                },
            ])
            .await
            .unwrap();
        assert_eq!(updated, 2);

        // Unknown room is rejected.
        assert!(
            store
                .set_layout(vec![DeviceLayout {
                    id: "a".into(),
                    room_id: Some(999),
                    position: 0,
                }])
                .await
                .is_err()
        );

        // Ordering follows position; deleting a room unassigns its devices.
        let ids: Vec<String> = store
            .list_devices()
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(ids, ["b", "a"]);
        assert!(store.delete_room(living.id).await.unwrap());
        let layout = store.list_layout().await.unwrap();
        let a = layout.iter().find(|l| l.id == "a").unwrap();
        assert_eq!(a.room_id, None);
        assert_eq!(a.position, 5);
    }

    #[tokio::test]
    async fn presets_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let preset = store
            .save_preset(Preset {
                id: 0,
                name: "Sunset".into(),
                command: LightCommand {
                    hue: Some(20),
                    saturation: Some(90),
                    brightness: Some(60),
                    ..Default::default()
                },
                position: 0,
            })
            .await
            .unwrap();
        assert!(preset.id > 0);
        let renamed = store
            .save_preset(Preset {
                name: "Evening".into(),
                ..preset.clone()
            })
            .await
            .unwrap();
        assert_eq!(store.list_presets().await.unwrap()[0], renamed);
        assert!(store.delete_preset(preset.id).await.unwrap());
        assert!(!store.delete_preset(preset.id).await.unwrap());
    }

    #[tokio::test]
    async fn usage_samples() {
        let store = Store::open_in_memory().unwrap();
        store.insert_device(device("a", "10.0.0.1")).await.unwrap();
        let sample = |ts: i64, wh: i64| UsageSample {
            device_id: "a".into(),
            ts,
            time_today: Some(10),
            time_past7: Some(100),
            time_past30: Some(400),
            power_today: Some(wh),
            power_past7: Some(wh * 7),
            power_past30: Some(wh * 30),
        };
        store.insert_usage_sample(sample(100, 5)).await.unwrap();
        store.insert_usage_sample(sample(200, 6)).await.unwrap();
        store.insert_usage_sample(sample(200, 7)).await.unwrap(); // same ts replaces
        assert!(
            store
                .insert_usage_sample(UsageSample {
                    device_id: "ghost".into(),
                    ..sample(300, 1)
                })
                .await
                .is_err()
        );

        let all = store.list_usage(None, 0).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].power_today, Some(7));
        assert_eq!(
            store.list_usage(Some("a".into()), 150).await.unwrap().len(),
            1
        );
        assert_eq!(
            store.list_usage(Some("b".into()), 0).await.unwrap().len(),
            0
        );

        let latest = store.latest_usage().await.unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].ts, 200);

        assert_eq!(store.prune_usage(150).await.unwrap(), 1);
        store.delete_device("a".into()).await.unwrap();
        assert!(store.list_usage(None, 0).await.unwrap().is_empty());
    }
}
