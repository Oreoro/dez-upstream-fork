use std::{collections::HashMap, path::Path};

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use sqlez::connection::Connection;
use uuid::Uuid;

use crate::{
    HostSessionSnapshot, MutationRequest, ProjectId, ProjectSpec, WorkspaceId, WorkspaceSnapshot,
};

pub struct HostSessionDb {
    connection: Mutex<Connection>,
}

impl HostSessionDb {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating session database directory {parent:?}"))?;
        }
        let connection = Connection::open_file(&path.to_string_lossy());
        Self::from_connection(connection)
    }

    #[cfg(test)]
    fn in_memory(name: &str) -> Result<Self> {
        Self::from_connection(Connection::open_memory(Some(name)))
    }

    fn from_connection(connection: Connection) -> Result<Self> {
        connection.exec(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS host_session (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                 revision INTEGER NOT NULL,
                 next_project_id INTEGER NOT NULL,
                 active_workspace_id TEXT NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS projects (
                 id INTEGER PRIMARY KEY,
                 spec_json TEXT NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS workspaces (
                 id TEXT PRIMARY KEY,
                 position INTEGER NOT NULL UNIQUE,
                 project_id INTEGER NOT NULL UNIQUE REFERENCES projects(id),
                 layout_revision INTEGER NOT NULL,
                 layout_json TEXT NOT NULL
             ) STRICT;",
        )?()
        .context("creating host session schema")?;
        let has_next_project_id = connection.select::<String>(
            "SELECT name FROM pragma_table_info('host_session')
                 WHERE name = 'next_project_id'",
        )?()?
        .into_iter()
        .next()
        .is_some();
        if !has_next_project_id {
            connection.exec(
                "ALTER TABLE host_session
                 ADD COLUMN next_project_id INTEGER NOT NULL DEFAULT 1;",
            )?()
            .context("adding host session project id allocator")?;
            connection.exec(
                "UPDATE host_session
                 SET next_project_id = COALESCE((SELECT MAX(id) + 1 FROM projects), 1);",
            )?()
            .context("initializing host session project id allocator")?;
        }
        let has_workspace_name = connection.select::<String>(
            "SELECT name FROM pragma_table_info('workspaces')
                 WHERE name = 'name'",
        )?()?
        .into_iter()
        .next()
        .is_some();
        if has_workspace_name {
            connection.exec("ALTER TABLE workspaces DROP COLUMN name;")?()
                .context("removing deferred workspace name column")?;
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn load(&self) -> Result<Option<HostSessionSnapshot>> {
        let connection = self.connection.lock();
        let mut session_rows = connection.select::<(i64, i64, String)>(
            "SELECT revision, next_project_id, active_workspace_id
             FROM host_session WHERE singleton = 1",
        )?;
        let Some((revision, next_project_id, active_workspace_id)) =
            session_rows()?.into_iter().next()
        else {
            return Ok(None);
        };

        let projects = connection.select::<(i64, String)>("SELECT id, spec_json FROM projects")?()?
            .into_iter()
            .map(|(id, json)| {
                let id = u64::try_from(id).context("project id is negative")?;
                let id = ProjectId::new(id).context("project id is zero")?;
                let spec = serde_json::from_str::<ProjectSpec>(&json)
                    .context("deserializing project specification")?;
                Ok((id, spec))
            })
            .collect::<Result<HashMap<_, _>>>()?;

        let rows = connection.select::<(String, i64, i64, i64, String)>(
            "SELECT id, position, project_id, layout_revision, layout_json
                 FROM workspaces ORDER BY position ASC",
        )?()?;
        let mut workspaces = Vec::with_capacity(rows.len());
        for (id, _position, project_id, layout_revision, layout_json) in rows {
            let workspace_id = WorkspaceId::from_uuid(
                Uuid::parse_str(&id).context("parsing persisted workspace id")?,
            );
            let project_id = ProjectId::new(
                u64::try_from(project_id).context("persisted project id is negative")?,
            )
            .context("persisted project id is zero")?;
            let project_spec = projects
                .get(&project_id)
                .cloned()
                .context("workspace references an absent project")?;
            let layout = serde_json::from_str(&layout_json)
                .context("deserializing persisted workspace layout")?;
            workspaces.push(WorkspaceSnapshot {
                id: workspace_id,
                project_id,
                project_spec,
                layout_revision: u64::try_from(layout_revision)
                    .context("layout revision is negative")?,
                layout,
            });
        }

        let snapshot = HostSessionSnapshot {
            revision: u64::try_from(revision).context("session revision is negative")?,
            next_project_id: u64::try_from(next_project_id)
                .context("next project id is negative")?,
            active_workspace_id: WorkspaceId::from_uuid(
                Uuid::parse_str(&active_workspace_id).context("parsing active workspace id")?,
            ),
            workspaces,
        };
        snapshot
            .validate()
            .context("validating persisted host session")?;
        Ok(Some(snapshot))
    }

    pub fn save(&self, snapshot: &HostSessionSnapshot) -> Result<()> {
        snapshot
            .validate()
            .context("validating host session before save")?;
        let revision = i64::try_from(snapshot.revision).context("session revision is too large")?;
        let next_project_id =
            i64::try_from(snapshot.next_project_id).context("next project id is too large")?;
        let connection = self.connection.lock();
        connection.with_savepoint("save_host_session", || {
            connection.exec("DELETE FROM workspaces")?()?;
            connection.exec("DELETE FROM projects")?()?;

            let mut insert_project = connection.exec_bound::<(i64, String)>(
                "INSERT INTO projects (id, spec_json) VALUES (?, ?)",
            )?;
            let mut insert_workspace = connection.exec_bound::<(String, i64, i64, i64, String)>(
                "INSERT INTO workspaces
                 (id, position, project_id, layout_revision, layout_json)
                 VALUES (?, ?, ?, ?, ?)",
            )?;

            for (position, workspace) in snapshot.workspaces.iter().enumerate() {
                let project_id =
                    i64::try_from(workspace.project_id.get()).context("project id is too large")?;
                insert_project((
                    project_id,
                    serde_json::to_string(&workspace.project_spec)
                        .context("serializing project specification")?,
                ))?;
                insert_workspace((
                    workspace.id.to_string(),
                    i64::try_from(position).context("workspace position is too large")?,
                    project_id,
                    i64::try_from(workspace.layout_revision)
                        .context("layout revision is too large")?,
                    serde_json::to_string(&workspace.layout)
                        .context("serializing workspace layout")?,
                ))?;
            }

            connection.exec_bound::<(i64, i64, String)>(
                "INSERT INTO host_session
                     (singleton, revision, next_project_id, active_workspace_id)
                     VALUES (1, ?, ?, ?)
                     ON CONFLICT(singleton) DO UPDATE SET
                         revision = excluded.revision,
                         next_project_id = excluded.next_project_id,
                         active_workspace_id = excluded.active_workspace_id",
            )?((
                revision,
                next_project_id,
                snapshot.active_workspace_id.to_string(),
            ))?;
            Ok(())
        })
    }

    pub fn commit_mutation(
        &self,
        snapshot: &mut HostSessionSnapshot,
        request: MutationRequest,
    ) -> Result<()> {
        let mut next_snapshot = snapshot.clone();
        next_snapshot
            .apply(request)
            .context("applying host session mutation")?;
        self.save(&next_snapshot)?;
        *snapshot = next_snapshot;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProjectRoot, SessionMutation};
    use std::path::PathBuf;

    #[test]
    fn database_round_trips_ordered_workspaces() {
        let database =
            HostSessionDb::in_memory("superzed-session-round-trip").expect("database should open");
        let mut snapshot = HostSessionSnapshot::default();
        snapshot
            .apply(MutationRequest {
                expected_revision: 0,
                mutation: SessionMutation::CreateWorkspace {
                    after: Some(snapshot.active_workspace_id),
                    project_spec: ProjectSpec {
                        roots: vec![ProjectRoot {
                            requested_path: PathBuf::from("/code/server"),
                            canonical_path: PathBuf::from("/code/server"),
                        }],
                    },
                },
            })
            .expect("workspace should be created");

        database.save(&snapshot).expect("snapshot should save");
        let restored = database
            .load()
            .expect("snapshot should load")
            .expect("snapshot should exist");
        assert_eq!(restored, snapshot);
    }

    #[test]
    fn migrates_database_without_project_id_allocator() {
        let connection = Connection::open_memory(Some("superzed-session-allocator-migration"));
        connection
            .exec(
                "CREATE TABLE host_session (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                    revision INTEGER NOT NULL,
                    active_workspace_id TEXT NOT NULL
                ) STRICT;",
            )
            .expect("legacy schema should prepare")()
        .expect("legacy schema should be created");

        let database =
            HostSessionDb::from_connection(connection).expect("legacy database should migrate");
        database
            .save(&HostSessionSnapshot::default())
            .expect("migrated database should save");
        assert!(database.load().expect("snapshot should load").is_some());
    }

    #[test]
    fn migrates_database_with_deferred_workspace_name_column() {
        let connection = Connection::open_memory(Some("superzed-session-name-migration"));
        connection
            .exec(
                "CREATE TABLE host_session (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                    revision INTEGER NOT NULL,
                    next_project_id INTEGER NOT NULL,
                    active_workspace_id TEXT NOT NULL
                ) STRICT;
                CREATE TABLE projects (
                    id INTEGER PRIMARY KEY,
                    spec_json TEXT NOT NULL
                ) STRICT;
                CREATE TABLE workspaces (
                    id TEXT PRIMARY KEY,
                    name TEXT,
                    position INTEGER NOT NULL UNIQUE,
                    project_id INTEGER NOT NULL UNIQUE REFERENCES projects(id),
                    layout_revision INTEGER NOT NULL,
                    layout_json TEXT NOT NULL
                ) STRICT;",
            )
            .expect("old schema should prepare")()
        .expect("old schema should be created");

        let database = HostSessionDb::from_connection(connection)
            .expect("workspace name column should migrate");
        let columns = database
            .connection
            .lock()
            .select::<String>("SELECT name FROM pragma_table_info('workspaces')")
            .expect("workspace columns query should prepare")()
        .expect("workspace columns should load");
        assert!(!columns.iter().any(|column| column == "name"));
    }

    #[test]
    fn failed_commit_rolls_back_database_and_authoritative_snapshot() {
        let database = HostSessionDb::in_memory("superzed-session-atomic-commit")
            .expect("database should open");
        let mut authoritative_snapshot = HostSessionSnapshot::default();
        database
            .save(&authoritative_snapshot)
            .expect("initial snapshot should save");
        let initial_snapshot = authoritative_snapshot.clone();

        database
            .connection
            .lock()
            .exec(
                "CREATE TRIGGER fail_second_workspace
                 BEFORE INSERT ON workspaces
                 WHEN NEW.position = 1
                 BEGIN
                     SELECT RAISE(ABORT, 'injected workspace insert failure');
                 END;",
            )
            .expect("failure trigger should prepare")()
        .expect("failure trigger should install");

        let expected_revision = authoritative_snapshot.revision;
        let active_workspace_id = authoritative_snapshot.active_workspace_id;
        let result = database.commit_mutation(
            &mut authoritative_snapshot,
            MutationRequest {
                expected_revision,
                mutation: SessionMutation::CreateWorkspace {
                    after: Some(active_workspace_id),
                    project_spec: ProjectSpec::default(),
                },
            },
        );
        assert!(result.is_err());
        assert_eq!(authoritative_snapshot, initial_snapshot);

        database
            .connection
            .lock()
            .exec("DROP TRIGGER fail_second_workspace")
            .expect("failure trigger removal should prepare")()
        .expect("failure trigger should be removed");
        assert_eq!(
            database
                .load()
                .expect("snapshot should load after rollback")
                .expect("snapshot should remain present"),
            initial_snapshot
        );
    }
}
