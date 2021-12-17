use crate::mysql_connector::{MySqlBinlogConnector, MySqlReplicator};
use crate::postgres_connector::{
    PostgresPosition, PostgresReplicator, PostgresWalConnector, PUBLICATION_NAME, REPLICATION_SLOT,
};
use async_trait::async_trait;
use futures::FutureExt;
use launchpad::select;
use metrics::{counter, histogram};
use mysql_async as mysql;
use noria::consensus::Authority;
use noria::consistency::Timestamp;
use noria::metrics::recorded::{self, SnapshotStatusTag};
use noria::replication::{ReplicationOffset, ReplicationOffsets};
use noria::{ControllerHandle, ReadySetError, ReadySetResult, Table, TableOperation};
use std::collections::{hash_map, HashMap, HashSet};
use std::convert::TryInto;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Notify;
use tokio_postgres as pgsql;
use tracing::{debug, error, info, info_span, warn, Instrument};

#[derive(Debug)]
pub(crate) enum ReplicationAction {
    TableAction {
        table: String,
        actions: Vec<TableOperation>,
        /// The transaction id of a table write operation. Each
        /// table write operation within a transaction should be assigned
        /// the same transaction id. These id's should be monotonically
        /// increasing across transactions.
        txid: Option<u64>,
    },
    SchemaChange {
        ddl: String,
    },
    LogPosition,
}

#[async_trait]
pub(crate) trait Connector {
    /// Process logical replication events until an actionable event occurs, returning
    /// the corresponding action.
    ///
    /// # Arguments
    ///
    /// * `last_pos` - the last processed position to. This is used only by Postgres to
    /// advance the replication slot position on the server.
    ///
    /// * `until` - an optional position in the binlog to stop at, even if no actionable
    /// occured. In that case the action [`ReplicationAction::LogPosition`] is returned.
    /// Currently this is only used by MySQL replicator while catching up on replication.
    ///
    async fn next_action(
        &mut self,
        last_pos: &ReplicationOffset,
        until: Option<&ReplicationOffset>,
    ) -> ReadySetResult<(ReplicationAction, ReplicationOffset)>;
}

/// An adapter that converts database events into Noria API calls
pub struct NoriaAdapter {
    /// The Noria API handle
    noria: ControllerHandle,
    /// The binlog reader
    connector: Box<dyn Connector + Send + Sync>,
    /// A map of cached table mutators
    mutator_map: HashMap<String, Table>,
    /// A HashSet of tables we've already warned about not existing
    warned_missing_tables: HashSet<String>,
    /// The set of replication offsets for the schema and the tables, obtained from the controller
    /// at startup and maintained during replication.
    ///
    /// Since some tables may have lagged behind others due to failed snapshotting, we start
    /// replication at the *minimum* replication offset, but ignore any replication events that come
    /// before the offset for that table
    replication_offsets: ReplicationOffsets,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum AdapterOpts {
    MySql(mysql::Opts),
    Postgres(pgsql::Config),
}

impl FromStr for AdapterOpts {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.starts_with("mysql://") {
            let opts: mysql::Opts = s.parse().map_err(|e: mysql::UrlError| e.to_string())?;
            if opts.db_name().is_none() {
                return Err("Database name is required in MySQL URL".to_string());
            }
            Ok(AdapterOpts::MySql(opts))
        } else if s.starts_with("postgres://") || s.starts_with("postgresql://") {
            let opts: pgsql::Config = s.parse().map_err(|e: pgsql::Error| e.to_string())?;
            if opts.get_dbname().is_none() {
                return Err("Database name is required in PostgreSQL URL".to_string());
            }
            Ok(AdapterOpts::Postgres(opts))
        } else {
            Err("A valid URL should begin with mysql:// or postgresql://".to_string())
        }
    }
}

impl NoriaAdapter {
    pub async fn start_with_authority(
        authority: Authority,
        options: AdapterOpts,
    ) -> ReadySetResult<!> {
        let noria = noria::ControllerHandle::new(authority).await;
        NoriaAdapter::start_inner(noria, options, None, None).await
    }
}

impl NoriaAdapter {
    /// Same as [`start`](Builder::start), but accepts a MySQL/PostgreSQL url for options
    /// and an externally supplied Noria `ControllerHandle`.
    /// The MySQL url must contain the database name, and user and password if applicable.
    /// i.e. `mysql://user:pass%20word@localhost/database_name` or
    /// `postgresql://user:pass%20word@localhost/database_name`
    #[allow(dead_code)]
    pub async fn start_with_url<U: AsRef<str>>(
        url: U,
        noria: ControllerHandle,
        server_id: Option<u32>,
        ready_notify: Option<Arc<Notify>>,
    ) -> ReadySetResult<!> {
        let options = url
            .as_ref()
            .parse()
            .map_err(|e| ReadySetError::ReplicationFailed(format!("Invalid URL format: {}", e)))?;

        NoriaAdapter::start_inner(noria, options, server_id, ready_notify)
            .instrument(info_span!("replicator"))
            .await
    }

    async fn start_inner(
        noria: ControllerHandle,
        options: AdapterOpts,
        server_id: Option<u32>,
        ready_notify: Option<Arc<Notify>>,
    ) -> ReadySetResult<!> {
        match options {
            AdapterOpts::MySql(options) => {
                NoriaAdapter::start_inner_mysql(options, noria, server_id, ready_notify).await
            }
            AdapterOpts::Postgres(options) => {
                NoriaAdapter::start_inner_postgres(options, noria, ready_notify).await
            }
        }
    }

    /// Finish the build and begin monitoring the binlog for changes
    /// If noria has no replication offset information, it will replicate the target database in its
    /// entirety to Noria before listening on the binlog
    /// The replication happens in stages:
    /// * READ LOCK is acquired on the database
    /// * Next binlog position is read
    /// * The recipe (schema) DDL is replicated and installed in Noria (replacing current recipe)
    /// * Each table is individually replicated into Noria
    /// * READ LOCK is released
    /// * Adapter keeps reading binlog from the next position keeping Noria up to date
    async fn start_inner_mysql(
        mysql_options: mysql::Opts,
        mut noria: ControllerHandle,
        server_id: Option<u32>,
        ready_notify: Option<Arc<Notify>>,
    ) -> ReadySetResult<!> {
        use crate::mysql_connector::BinlogPosition;
        // Load the replication offset for all tables and the schema from Noria
        let mut replication_offsets = noria.replication_offsets().await?;
        let pos = match replication_offsets.max_offset()? {
            None => {
                let span = info_span!("taking database snapshot");
                let replicator_options = mysql_options.clone();
                let pool = mysql::Pool::new(replicator_options);
                let replicator = MySqlReplicator { pool, tables: None };

                let snapshot_start = Instant::now();
                counter!(
                    recorded::REPLICATOR_SNAPSHOT_STATUS,
                    1u64,
                    "status" => SnapshotStatusTag::Started.value(),
                );
                span.in_scope(|| info!("Starting snapshot"));
                let curr_offset = replicator
                    .snapshot_to_noria(&mut noria, &replication_offsets, true)
                    .instrument(span.clone())
                    .await;

                let status = if curr_offset.is_err() {
                    SnapshotStatusTag::Failed.value()
                } else {
                    SnapshotStatusTag::Successful.value()
                };

                counter!(
                    recorded::REPLICATOR_SNAPSHOT_STATUS,
                    1u64,
                    "status" => status
                );

                // Get updated offests, after potential replication happened
                replication_offsets = noria.replication_offsets().await?;

                // If we have some offsets in `replication_offsets`, that means some tables were
                // already snapshot before we started up. But if we're in this block
                // (`max_offsets()` returned `None`), that means not *all* tables were already
                // snapshot before we started up. So we've got some tables at an old offset that
                // need to catch up to the just-snapshotted tables at `curr_offset`. We discard
                // replication events for offsets < the replication offset of that table, so we can
                // do this "catching up" by just starting replication at the old offset.
                //
                // Otherwise, if we *don't* have any offsets in `replication_offsets` (in which case
                // `min_present_offset` would return `None`), we know we just did a full snapshot,
                // in which case we can just start replicating at the current offset
                let pos = replication_offsets
                    .min_present_offset()?
                    .map(|off| Ok(off.clone().into()))
                    .unwrap_or(curr_offset)?;

                span.in_scope(|| info!("Snapshot finished"));
                histogram!(
                    recorded::REPLICATOR_SNAPSHOT_DURATION,
                    snapshot_start.elapsed().as_micros() as f64
                );
                pos
            }
            Some(pos) => pos.clone().into(),
        };

        let schemas = mysql_options
            .db_name()
            .map(|s| vec![s.to_string()])
            .unwrap_or_default();

        // TODO: it is possible that the binlog position from noria is no longer
        // present on the primary, in which case the connection will fail, and we would
        // need to perform a new snapshot
        let connector = Box::new(
            MySqlBinlogConnector::connect(mysql_options, schemas, pos.clone(), server_id).await?,
        );

        let mut adapter = NoriaAdapter {
            noria,
            connector,
            replication_offsets,
            mutator_map: HashMap::new(),
            warned_missing_tables: HashSet::new(),
        };

        let mut current_pos: ReplicationOffset = pos.try_into()?;

        // At this point it is possible that we just finished replication, but
        // our schema and our tables are taken at different position in the binlog.
        // Until our database has a consitent view of the database at a single point
        // in time, it is not safe to issue any queries. We therefore advance the binlog
        // to the position of the most recent table we have, applying changes as needed.
        // Only once binlog advanced to that point, can we send a ready signal to noria.
        match adapter.replication_offsets.max_offset()? {
            Some(max) if max > &current_pos => {
                info!(
                    start = ?BinlogPosition::from(&current_pos),
                    end = ?BinlogPosition::from(max),
                    "Catching up");
                let max = max.clone();
                adapter.main_loop(&mut current_pos, Some(max)).await?;
            }
            _ => {}
        }

        info!("MySQL connected");
        info!(binlog_position = ?BinlogPosition::from(&current_pos));

        // Let waiters know that the initial snapshotting is complete.
        if let Some(notify) = ready_notify {
            notify.notify_one();
        }

        adapter.main_loop(&mut current_pos, None).await?;

        unreachable!("`main_loop` will never stop with an Ok status if `until = None`");
    }

    async fn start_inner_postgres(
        pgsql_opts: pgsql::Config,
        mut noria: ControllerHandle,
        ready_notify: Option<Arc<Notify>>,
    ) -> ReadySetResult<!> {
        // Attempt to retreive the latest replication offset from noria, if none is present
        // begin the snapshot process
        let replication_offsets = noria.replication_offsets().await?;
        let pos = replication_offsets.max_offset()?.map(Into::into);

        if let Some(pos) = pos {
            info!(wal_position = %pos);
        }

        let dbname = pgsql_opts
            .get_dbname()
            .map(|s| vec![s.to_string()])
            .unwrap_or_default();

        let mut connector = Box::new(
            PostgresWalConnector::connect(pgsql_opts.clone(), dbname.first().unwrap(), pos).await?,
        );

        info!("Connected to PostgreSQL");

        if let Some(snapshot) = connector.snapshot_name.as_deref() {
            // If snapshot name exists, it means we need to make a snapshot to noria
            let (mut client, connection) = pgsql_opts
                .connect(postgres_native_tls::MakeTlsConnector::new(
                    native_tls::TlsConnector::builder().build().unwrap(),
                ))
                .await?;

            let connection_handle = tokio::spawn(connection);

            let mut replicator = PostgresReplicator::new(&mut client, &mut noria, None).await?;

            select! {
                s = replicator.snapshot_to_noria(snapshot).fuse() => s?,
                c = connection_handle.fuse() => c.unwrap()?,
            }

            info!("Snapshot finished");
        }

        // Let waiters know that the initial snapshotting is complete.
        if let Some(notify) = ready_notify {
            notify.notify_one();
        }

        connector
            .start_replication(REPLICATION_SLOT, PUBLICATION_NAME)
            .await?;

        info!("Streaming replication started");

        let mut adapter = NoriaAdapter {
            noria,
            connector,
            replication_offsets,
            mutator_map: HashMap::new(),
            warned_missing_tables: HashSet::new(),
        };

        adapter
            .main_loop(&mut PostgresPosition::default().into(), None)
            .await?;

        unreachable!("`main_loop` will never stop with an Ok status if `until = None`");
    }

    /// Handle a single BinlogAction by calling the proper Noria RPC
    async fn handle_action(
        &mut self,
        action: ReplicationAction,
        pos: ReplicationOffset,
    ) -> ReadySetResult<()> {
        match action {
            ReplicationAction::SchemaChange { ddl } => {
                if let Some(schema_offset) = &self.replication_offsets.schema {
                    if pos < *schema_offset {
                        debug!(
                            ?pos,
                            ?schema_offset,
                            "Skipping schema change action from before the schema offset"
                        );
                        return Ok(());
                    }
                }

                // Send the query to Noria as is
                self.noria
                    .extend_recipe_with_offset(&ddl, pos.clone(), false)
                    .await?;
                self.replication_offsets.schema = Some(pos);
                self.clear_mutator_cache();
            }

            ReplicationAction::TableAction {
                table,
                mut actions,
                txid,
            } => {
                if let Some(Some(table_offset)) = self.replication_offsets.tables.get(&table) {
                    if pos < *table_offset {
                        debug!(
                            ?pos,
                            ?table_offset,
                            %table,
                            "Skipping table action from before the table's offset"
                        );
                        return Ok(());
                    }
                }

                // Send the rows as are
                let table_mutator =
                    if let Some(table) = self.mutator_for_table(table.clone()).await? {
                        table
                    } else {
                        if self.warned_missing_tables.insert(table.clone()) {
                            warn!(
                                table_name = %table,
                                num_actions = actions.len(),
                                "Could not find table, discarding actions"
                            );
                        }
                        return Ok(());
                    };
                actions.push(TableOperation::SetReplicationOffset(pos.clone()));
                table_mutator.perform_all(actions).await?;

                // If there was a transaction id associated, propagate the
                // timestamp with that transaction id
                // TODO(justin): Make this operation atomic with the table
                // actions being pushed above.
                if let Some(tx) = txid {
                    let mut timestamp = Timestamp::default();
                    timestamp.map.insert(table_mutator.node, tx);
                    table_mutator.update_timestamp(timestamp).await?;
                }

                self.replication_offsets.tables.insert(table, Some(pos));
            }

            ReplicationAction::LogPosition => {
                self.replication_offsets.set_offset(pos.clone());

                // Update the log position for the schema
                self.noria
                    .set_schema_replication_offset(Some(pos.clone()))
                    .await?;

                // Update the log position for all tables
                let tables = self
                    .replication_offsets
                    .tables
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                for table in tables {
                    if let Some(table) = self.mutator_for_table(table).await? {
                        table.set_replication_offset(pos.clone()).await?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Loop over the actions
    async fn main_loop(
        &mut self,
        position: &mut ReplicationOffset,
        until: Option<ReplicationOffset>,
    ) -> ReadySetResult<()> {
        loop {
            if until.as_ref().map(|u| *position >= *u).unwrap_or(false) {
                return Ok(());
            }

            let (action, pos) = self.connector.next_action(position, until.as_ref()).await?;
            *position = pos.clone();

            debug!(?action);

            match self.handle_action(action, pos).await {
                // ReadySet likely entered an invalid state fail the replicator.
                Err(e @ ReadySetError::RecipeInvariantViolated(_)) => return Err(e),
                Err(err) => error!(error = %err),
                _ => {}
            }
        }
    }

    /// When schema changes there is a risk the cached mutators will no longer be in sync
    /// and we need to drop them all
    fn clear_mutator_cache(&mut self) {
        self.mutator_map.clear()
    }

    /// Get a mutator for a noria table from the cache if available, or fetch a new one
    /// from the controller and cache it. Returns None if the table doesn't exist in noria.
    async fn mutator_for_table(&mut self, name: String) -> ReadySetResult<Option<&mut Table>> {
        match self.mutator_map.entry(name) {
            hash_map::Entry::Occupied(o) => Ok(Some(o.into_mut())),
            hash_map::Entry::Vacant(v) => match self.noria.table(v.key()).await {
                Ok(table) => Ok(Some(v.insert(table))),
                Err(e) if e.caused_by_table_not_found() => Ok(None),
                Err(e) => Err(e),
            },
        }
    }
}
