use crate::model::{identity, Edge, FileFacts, Node, Snapshot, SCHEMA_VERSION};
use crate::source::SourceSet;
use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Index {
    pub connection: Connection,
    pub path: PathBuf,
}

impl crate::frontend::FrontendCache for Index {
    fn load_record(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row("SELECT data FROM syntax_cache WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    fn store_record(&self, key: &str, record: &str) -> Result<()> {
        self.connection.execute(
            "INSERT OR REPLACE INTO syntax_cache(key,data) VALUES(?1,?2)",
            params![key, record],
        )?;
        Ok(())
    }
}

impl Index {
    pub fn open(root: &Path, cache_dir: Option<&Path>) -> Result<Self> {
        let default_dir;
        let cache = match cache_dir {
            Some(path) => path,
            None => {
                default_dir = ProjectDirs::from("dev", "codexis", "codexis")
                    .context("cannot find user cache directory; use --cache-dir")?
                    .cache_dir()
                    .to_owned();
                &default_dir
            }
        };
        let project_key = identity(&[&root.canonicalize()?.to_string_lossy()]);
        let directory = cache.join(&project_key[..24]);
        std::fs::create_dir_all(&directory)?;
        let path = directory.join("index.sqlite3");
        let connection = Connection::open(&path)?;
        connection.busy_timeout(Duration::from_secs(10))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        // Bulk snapshots touch several large B-trees. The SQLite default page
        // cache repeatedly spills dirty pages while inserting a large graph.
        // This is a KiB budget (negative value), allocated on demand.
        connection.pragma_update(None, "cache_size", -131072)?;
        let version: u32 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != 0 && version != SCHEMA_VERSION {
            bail!("unsupported cache schema {version}; use a new --cache-dir");
        }
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS blobs(hash TEXT PRIMARY KEY, content TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS syntax_cache(key TEXT PRIMARY KEY, data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS snapshots(id TEXT PRIMARY KEY, created INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS snapshot_files(snapshot TEXT, path TEXT, hash TEXT, PRIMARY KEY(snapshot,path));
             CREATE TABLE IF NOT EXISTS nodes(snapshot TEXT, id TEXT, package TEXT, unit TEXT, path TEXT, name TEXT,
                qualified TEXT, kind TEXT, start INTEGER, end INTEGER, data TEXT NOT NULL, PRIMARY KEY(snapshot,id));
             CREATE INDEX IF NOT EXISTS nodes_lookup_qualified ON nodes(snapshot,qualified,path,start);
             CREATE INDEX IF NOT EXISTS nodes_lookup_name ON nodes(snapshot,name,qualified,path,start);
             CREATE INDEX IF NOT EXISTS nodes_file ON nodes(snapshot,path,start);
             CREATE INDEX IF NOT EXISTS nodes_package ON nodes(snapshot,package,kind);
             CREATE TABLE IF NOT EXISTS edges(snapshot TEXT, id TEXT, source TEXT, target TEXT, kind TEXT, data TEXT NOT NULL,
                PRIMARY KEY(snapshot,id));
             CREATE INDEX IF NOT EXISTS edges_read_source ON edges(snapshot,source,id);
             CREATE INDEX IF NOT EXISTS edges_read_target ON edges(snapshot,target,id);
             CREATE TABLE IF NOT EXISTS marks(stable_key TEXT PRIMARY KEY, snapshot TEXT, fingerprint TEXT, state TEXT,
                note TEXT, updated INTEGER);"
        )?;
        // Superseded indexes held the same leading keys but were not used by
        // our bounded/sorted lookups. Keep data and all supported query indexes.
        connection.execute_batch(
            "DROP INDEX IF EXISTS nodes_name;
             DROP INDEX IF EXISTS nodes_qualified;
             DROP INDEX IF EXISTS edges_source;
             DROP INDEX IF EXISTS edges_target;",
        )?;
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self { connection, path })
    }

    pub fn cached(&self, key: &str) -> Result<Option<FileFacts>> {
        let data: Option<String> = self
            .connection
            .query_row("SELECT data FROM syntax_cache WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        data.map(|s| serde_json::from_str(&s).context("read cached syntax"))
            .transpose()
    }

    pub fn cache(&self, key: &str, facts: &FileFacts) -> Result<()> {
        self.connection.execute(
            "INSERT OR REPLACE INTO syntax_cache(key,data) VALUES(?1,?2)",
            params![key, serde_json::to_string(facts)?],
        )?;
        Ok(())
    }

    pub fn publish(
        &mut self,
        snapshot: &Snapshot,
        sources: &SourceSet,
        facts: &FileFacts,
    ) -> Result<()> {
        let tx = self.connection.transaction()?;
        let first_snapshot: bool = tx.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM snapshots LIMIT 1)",
            [],
            |r| r.get(0),
        )?;
        // On a fresh cache, build secondary B-trees in bulk after inserting the
        // graph. Doing so inside this transaction keeps publication atomic.
        if first_snapshot {
            tx.execute_batch(
                "DROP INDEX nodes_lookup_qualified;
                DROP INDEX nodes_lookup_name; DROP INDEX nodes_file; DROP INDEX nodes_package;
                DROP INDEX edges_read_source; DROP INDEX edges_read_target;",
            )?;
        }
        let id = &snapshot.id;
        tx.execute("DELETE FROM nodes WHERE snapshot=?1", [id])?;
        tx.execute("DELETE FROM edges WHERE snapshot=?1", [id])?;
        tx.execute("DELETE FROM snapshot_files WHERE snapshot=?1", [id])?;
        {
            let mut blob = tx.prepare("INSERT OR IGNORE INTO blobs(hash,content) VALUES(?1,?2)")?;
            let mut file =
                tx.prepare("INSERT INTO snapshot_files(snapshot,path,hash) VALUES(?1,?2,?3)")?;
            for source in sources.files.values() {
                blob.execute(params![source.hash, source.content])?;
                file.execute(params![id, source.path, source.hash])?;
            }
            let mut statement = tx.prepare("INSERT OR REPLACE INTO nodes(snapshot,id,package,unit,path,name,qualified,kind,start,end,data) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)")?;
            let mut ordered_nodes: Vec<_> = facts.nodes.iter().collect();
            ordered_nodes.sort_by(|a, b| a.id.cmp(&b.id));
            for node in ordered_nodes {
                statement.execute(params![
                    id,
                    node.id,
                    node.package,
                    node.unit,
                    node.evidence.path,
                    node.name,
                    node.qualified_name,
                    node.kind,
                    node.evidence.start_byte,
                    node.evidence.end_byte,
                    serde_json::to_string(node)?
                ])?;
            }
            let mut statement = tx.prepare("INSERT OR REPLACE INTO edges(snapshot,id,source,target,kind,data) VALUES(?1,?2,?3,?4,?5,?6)")?;
            let mut ordered_edges: Vec<_> = facts.edges.iter().collect();
            ordered_edges.sort_by(|a, b| a.id.cmp(&b.id));
            for edge in ordered_edges {
                statement.execute(params![
                    id,
                    edge.id,
                    edge.source,
                    edge.target,
                    edge.kind,
                    serde_json::to_string(edge)?
                ])?;
            }
        }
        if first_snapshot {
            tx.execute_batch(
                "CREATE INDEX nodes_lookup_qualified ON nodes(snapshot,qualified,path,start);
                CREATE INDEX nodes_lookup_name ON nodes(snapshot,name,qualified,path,start);
                CREATE INDEX nodes_file ON nodes(snapshot,path,start);
                CREATE INDEX nodes_package ON nodes(snapshot,package,kind);
                CREATE INDEX edges_read_source ON edges(snapshot,source,id);
                CREATE INDEX edges_read_target ON edges(snapshot,target,id);",
            )?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO snapshots(id,created,data) VALUES(?1,?2,?3)",
            params![id, snapshot.created_at_ms, serde_json::to_string(snapshot)?],
        )?;
        crate::source::check_cancelled()?;
        tx.commit()?;
        Ok(())
    }

    pub fn snapshot(&self, id: Option<&str>) -> Result<Snapshot> {
        let data: Option<String> = match id {
            Some(id) => self
                .connection
                .query_row("SELECT data FROM snapshots WHERE id=?1", [id], |r| r.get(0))
                .optional()?,
            None => self
                .connection
                .query_row(
                    "SELECT data FROM snapshots ORDER BY created DESC,id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?,
        };
        serde_json::from_str(
            &data.context("no completed snapshot found; run codexis analyze first")?,
        )
        .context("read snapshot")
    }

    pub fn find_nodes(&self, snapshot: &str, query: &str, limit: usize) -> Result<Vec<Node>> {
        let mut output = Vec::new();
        // Separate indexed lookups: an OR across these columns made SQLite scan
        // every node in the snapshot before sorting, even for an exact name.
        for column in ["id", "qualified", "name"] {
            let access = match column {
                "qualified" => " INDEXED BY nodes_lookup_qualified",
                "name" => " INDEXED BY nodes_lookup_name",
                _ => "",
            };
            let sql = format!("SELECT data FROM nodes{access} WHERE snapshot=?1 AND {column}=?2 ORDER BY qualified,path,start LIMIT ?3");
            let mut statement = self.connection.prepare(&sql)?;
            let rows =
                statement.query_map(params![snapshot, query, limit], |r| r.get::<_, String>(0))?;
            for row in rows {
                output.push(serde_json::from_str(&row?)?);
            }
            if !output.is_empty() {
                return Ok(output);
            }
        }
        let escaped = query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let mut statement = self.connection.prepare("SELECT data FROM nodes WHERE snapshot=?1 AND (qualified LIKE ?2 ESCAPE '\\' OR id LIKE ?3 ESCAPE '\\') ORDER BY qualified,path,start LIMIT ?4")?;
        let rows = statement.query_map(
            params![snapshot, pattern, format!("{escaped}%"), limit],
            |r| r.get::<_, String>(0),
        )?;
        for row in rows {
            output.push(serde_json::from_str(&row?)?);
        }
        Ok(output)
    }

    pub fn all_nodes(&self, snapshot: &str) -> Result<Vec<Node>> {
        self.read_json_rows(
            "SELECT data FROM nodes WHERE snapshot=?1 ORDER BY path,start,id",
            snapshot,
        )
    }

    /// Nodes of the given kinds only; avoids materializing variants, values
    /// and macros for whole-project passes that do not use them.
    pub fn nodes_of_kinds(
        &self,
        snapshot: &str,
        kinds: &[&str],
        trim: impl Fn(&mut Node) -> bool,
    ) -> Result<Vec<Node>> {
        let placeholders = vec!["?"; kinds.len()].join(",");
        let sql = format!(
            "SELECT data FROM nodes WHERE snapshot=?1 AND kind IN ({placeholders}) ORDER BY path,start,id"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let mut values: Vec<&dyn rusqlite::ToSql> = vec![&snapshot];
        for kind in kinds {
            values.push(kind);
        }
        let rows = statement.query_map(values.as_slice(), |r| r.get::<_, String>(0))?;
        let mut output = Vec::new();
        for row in rows {
            let mut node: Node = serde_json::from_str(&row?)?;
            if trim(&mut node) {
                output.push(node);
            }
        }
        Ok(output)
    }

    pub fn all_edges(&self, snapshot: &str) -> Result<Vec<Edge>> {
        self.read_json_rows(
            "SELECT data FROM edges WHERE snapshot=?1 ORDER BY source,id",
            snapshot,
        )
    }

    fn read_json_rows<T: serde::de::DeserializeOwned>(
        &self,
        sql: &str,
        snapshot: &str,
    ) -> Result<Vec<T>> {
        let mut statement = self.connection.prepare(sql)?;
        let rows = statement.query_map([snapshot], |r| r.get::<_, String>(0))?;
        let mut output = Vec::new();
        for row in rows {
            output.push(serde_json::from_str(&row?)?);
        }
        Ok(output)
    }

    pub fn connected_edges(
        &self,
        snapshot: &str,
        node: &str,
        incoming: bool,
        limit: usize,
    ) -> Result<Vec<Edge>> {
        self.related_edges(snapshot, node, incoming, limit, false)
    }

    pub fn connected_calls(
        &self,
        snapshot: &str,
        node: &str,
        incoming: bool,
        limit: usize,
    ) -> Result<Vec<Edge>> {
        self.related_edges(snapshot, node, incoming, limit, true)
    }

    fn related_edges(
        &self,
        snapshot: &str,
        node: &str,
        incoming: bool,
        limit: usize,
        calls_only: bool,
    ) -> Result<Vec<Edge>> {
        let column = if incoming { "target" } else { "source" };
        let access = if incoming {
            "edges_read_target"
        } else {
            "edges_read_source"
        };
        let filter = if calls_only { " AND kind='calls'" } else { "" };
        let sql = format!(
            "SELECT data FROM edges INDEXED BY {access} WHERE snapshot=?1 AND {column}=?2{filter} ORDER BY id LIMIT ?3"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let rows =
            statement.query_map(params![snapshot, node, limit], |r| r.get::<_, String>(0))?;
        let mut output = Vec::new();
        for row in rows {
            output.push(serde_json::from_str(&row?)?);
        }
        Ok(output)
    }

    pub fn content(&self, hash: &str) -> Result<String> {
        self.connection
            .query_row("SELECT content FROM blobs WHERE hash=?1", [hash], |r| {
                r.get(0)
            })
            .context("source evidence is unavailable")
    }

    pub fn file_hashes(
        &self,
        snapshot: &str,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        let mut statement = self
            .connection
            .prepare("SELECT path,hash FROM snapshot_files WHERE snapshot=?1 ORDER BY path")?;
        let rows = statement.query_map([snapshot], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}
