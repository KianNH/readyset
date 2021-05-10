use noria::{
    consistency::Timestamp, internal::LocalNodeIndex, ControllerHandle, DataType, ReadySetError,
    ReadySetResult, Table, TableOperation, View, ViewQuery, ViewQueryFilter, ViewQueryOperator,
    ZookeeperAuthority,
};

use msql_srv::{self, *};
use nom_sql::{
    self, BinaryOperator, ColumnConstraint, InsertStatement, Literal, SelectStatement, SqlQuery,
    UpdateStatement,
};
use vec1::vec1;

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::convert::{TryFrom, TryInto};
use std::sync::atomic;
use std::sync::{Arc, RwLock};

use crate::convert::ToDataType;
use crate::rewrite;
use crate::schema::{self, schema_for_column, Schema};
use crate::utils;

use crate::backend::error::Error;
use crate::backend::SelectSchema;
use itertools::Itertools;
use noria::errors::ReadySetError::PreparedStatementMissing;
use noria::errors::{internal_err, table_err, unsupported_err};
use noria::results::Results;
use noria::{internal, invariant_eq, unsupported};
use std::fmt;

type StatementID = u32;

#[derive(Clone)]
pub enum PreparedStatement {
    Select {
        name: String,
        statement: nom_sql::SelectStatement,
        schema: Vec<msql_srv::Column>,
        key_column_indices: Vec<usize>,
        rewritten_columns: Option<(usize, usize)>,
    },
    Insert(nom_sql::InsertStatement),
    Update(nom_sql::UpdateStatement),
}

impl fmt::Debug for PreparedStatement {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match self {
            PreparedStatement::Select {
                name, statement, ..
            } => write!(f, "{}: {}", name, statement),
            PreparedStatement::Insert(s) => write!(f, "{}", s),
            PreparedStatement::Update(s) => write!(f, "{}", s),
        }
    }
}

pub struct NoriaBackendInner {
    noria: ControllerHandle<ZookeeperAuthority>,
    inputs: BTreeMap<String, Table>,
    outputs: BTreeMap<String, View>,
}

macro_rules! noria_await {
    ($self:expr, $fut:expr) => {{
        let noria = &mut $self.noria;

        futures_util::future::poll_fn(|cx| noria.poll_ready(cx)).await?;
        $fut.await
    }};
}

impl NoriaBackendInner {
    async fn new(mut ch: ControllerHandle<ZookeeperAuthority>) -> Self {
        ch.ready().await.unwrap();
        let inputs = ch.inputs().await.expect("couldn't get inputs from Noria");
        let mut i = BTreeMap::new();
        for (n, _) in inputs {
            ch.ready().await.unwrap();
            let t = ch.table(&n).await.unwrap();
            i.insert(n, t);
        }
        ch.ready().await.unwrap();
        let outputs = ch.outputs().await.expect("couldn't get outputs from Noria");
        let mut o = BTreeMap::new();
        for (n, _) in outputs {
            ch.ready().await.unwrap();
            let t = ch.view(&n).await.unwrap();
            o.insert(n, t);
        }
        NoriaBackendInner {
            inputs: i,
            outputs: o,
            noria: ch,
        }
    }

    async fn ensure_mutator(&mut self, table: &str) -> ReadySetResult<&mut Table> {
        self.get_or_make_mutator(table).await
    }

    async fn ensure_getter(
        &mut self,
        view: &str,
        region: Option<String>,
    ) -> ReadySetResult<&mut View> {
        self.get_or_make_getter(view, region).await
    }

    async fn get_or_make_mutator(&mut self, table: &str) -> ReadySetResult<&mut Table> {
        if !self.inputs.contains_key(table) {
            let t = noria_await!(self, self.noria.table(table))?;
            self.inputs.insert(table.to_owned(), t);
        }
        Ok(self.inputs.get_mut(table).unwrap())
    }

    async fn get_or_make_getter(
        &mut self,
        view: &str,
        region: Option<String>,
    ) -> ReadySetResult<&mut View> {
        if !self.outputs.contains_key(view) {
            let vh = match region {
                None => noria_await!(self, self.noria.view(view))?,
                Some(r) => noria_await!(self, self.noria.view_from_region(view, r))?,
            };
            self.outputs.insert(view.to_owned(), vh);
        }
        Ok(self.outputs.get_mut(view).unwrap())
    }
}

pub struct NoriaConnector {
    inner: NoriaBackendInner,
    auto_increments: Arc<RwLock<HashMap<String, atomic::AtomicUsize>>>,
    /// global cache of view endpoints and prepared statements
    cached: Arc<RwLock<HashMap<SelectStatement, String>>>,
    /// thread-local version of `cached` (consulted first)
    tl_cached: HashMap<SelectStatement, String>,
    prepared_statement_cache: HashMap<StatementID, PreparedStatement>,
    /// The region to pass to noria for replica selection.
    region: Option<String>,
}

impl NoriaConnector {
    pub async fn new(
        ch: ControllerHandle<ZookeeperAuthority>,
        auto_increments: Arc<RwLock<HashMap<String, atomic::AtomicUsize>>>,
        query_cache: Arc<RwLock<HashMap<SelectStatement, String>>>,
        region: Option<String>,
    ) -> Self {
        NoriaConnector {
            inner: NoriaBackendInner::new(ch).await,
            auto_increments,
            cached: query_cache,
            tl_cached: HashMap::new(),
            prepared_statement_cache: HashMap::new(),
            region,
        }
    }

    // TODO(andrew): Allow client to map table names to NodeIndexes without having to query Noria
    // repeatedly. Eventually, this will be responsibility of the TimestampService.
    pub async fn node_index_of(&mut self, table_name: &str) -> Result<LocalNodeIndex, Error> {
        let table_handle = self.inner.noria.table(table_name).await?;
        Ok(table_handle.node)
    }
    pub async fn handle_insert(
        &mut self,
        mut q: nom_sql::InsertStatement,
    ) -> std::result::Result<(u64, u64), Error> {
        let table = &q.table.name;

        // create a mutator if we don't have one for this table already
        trace!(%table, "query::insert::access mutator");
        let putter = self.inner.ensure_mutator(table).await?;
        trace!("query::insert::extract schema");
        let schema = putter
            .schema()
            .ok_or_else(|| internal_err(format!("no schema for table '{}'", table)))?;

        // set column names (insert schema) if not set
        if q.fields.is_none() {
            q.fields = Some(schema.fields.iter().map(|cs| cs.column.clone()).collect());
        }

        let data: Vec<Vec<DataType>> = q
            .data
            .iter()
            .map(|row| row.iter().map(DataType::from).collect())
            .collect();

        self.do_insert(&q, data).await
    }

    pub async fn prepare_insert(
        &mut self,
        mut sql_q: nom_sql::SqlQuery,
        statement_id: u32,
    ) -> std::result::Result<(u32, Vec<msql_srv::Column>, Vec<Column>), Error> {
        let q = if let nom_sql::SqlQuery::Insert(ref q) = sql_q {
            q
        } else {
            internal!()
        };

        trace!(table = %q.table.name, "insert::access mutator");
        let mutator = self.inner.ensure_mutator(&q.table.name).await?;
        trace!("insert::extract schema");
        let schema = schema::convert_schema(&Schema::Table(mutator.schema().unwrap().clone()));

        match sql_q {
            // set column names (insert schema) if not set
            nom_sql::SqlQuery::Insert(ref mut q) => {
                if q.fields.is_none() {
                    q.fields = Some(
                        mutator
                            .schema()
                            .as_ref()
                            .unwrap()
                            .fields
                            .iter()
                            .map(|cs| cs.column.clone())
                            .collect(),
                    );
                }
            }
            _ => (),
        }

        let params: Vec<_> = {
            // extract parameter columns -- easy here, since they must all be in the same table
            let param_cols = utils::get_parameter_columns(&sql_q);
            param_cols
                .into_iter()
                .map(|c| {
                    //let mut cc = c.clone();
                    //cc.table = Some(q.table.name.clone());
                    //schema_for_column(table_schemas, &cc)
                    Ok(schema
                        .iter()
                        .cloned()
                        .find(|mc| c.name == mc.column)
                        .ok_or_else(|| {
                            internal_err(format!("column '{}' missing in mutator schema", c))
                        })?)
                })
                .collect::<ReadySetResult<Vec<_>>>()?
        };

        // nothing more to do for an insert
        // register a new prepared statement
        let q = if let nom_sql::SqlQuery::Insert(q) = sql_q {
            q
        } else {
            internal!()
        };
        trace!(id = statement_id, "insert::registered");
        self.prepared_statement_cache
            .insert(statement_id, PreparedStatement::Insert(q));
        Ok((statement_id, params, schema))
    }

    pub(crate) async fn execute_prepared_insert(
        &mut self,
        q_id: u32,
        params: Vec<DataType>,
    ) -> std::result::Result<(u64, u64), Error> {
        let prep: PreparedStatement = self
            .prepared_statement_cache
            .get(&q_id)
            .ok_or(PreparedStatementMissing)?
            .clone();
        trace!("delegate");
        match prep {
            PreparedStatement::Insert(ref q) => {
                let table = &q.table.name;
                let putter = self.inner.ensure_mutator(table).await?;
                trace!("insert::extract schema");
                let schema = putter
                    .schema()
                    .ok_or_else(|| internal_err(format!("no schema for table '{}'", table)))?;
                // unwrap: safe because we always pass in Some(params) so don't hit None path of coerce_params
                let coerced_params =
                    utils::coerce_params(Some(params), &SqlQuery::Insert(q.clone()), &schema)
                        .unwrap()
                        .unwrap();
                return self.do_insert(&q, vec![coerced_params]).await;
            }
            _ => {
                internal!(
                    "Execute_prepared_insert is being called for a non insert prepared statement."
                );
            }
        };
    }

    pub(crate) async fn handle_delete(
        &mut self,
        q: nom_sql::DeleteStatement,
    ) -> std::result::Result<u64, Error> {
        let cond = q
            .where_clause
            .ok_or_else(|| unsupported_err("only supports DELETEs with WHERE-clauses"))?;

        // create a mutator if we don't have one for this table already
        trace!(table = %q.table.name, "delete::access mutator");
        let mutator = self.inner.ensure_mutator(&q.table.name).await?;

        trace!("delete::extract schema");
        let pkey = if let Some(cts) = mutator.schema() {
            utils::get_primary_key(cts)
                .into_iter()
                .map(|(_, c)| c)
                .collect()
        } else {
            unsupported!("cannot delete from view");
        };

        trace!("delete::flatten conditionals");
        match utils::flatten_conditional(&cond, &pkey)? {
            None => Ok(0 as u64),
            Some(ref flattened) if flattened.is_empty() => {
                unsupported!("DELETE only supports WHERE-clauses on primary keys")
            }
            Some(flattened) => {
                let count = flattened.len() as u64;
                trace!("delete::execute");
                for key in flattened {
                    if let Err(e) = mutator.delete(key).await {
                        error!(error = %e, "failed");
                        Err(e)?
                    };
                }
                trace!("delete::done");
                Ok(count)
            }
        }
    }

    pub(crate) async fn handle_update(
        &mut self,
        q: nom_sql::UpdateStatement,
    ) -> std::result::Result<(u64, u64), Error> {
        self.do_update(Cow::Owned(q), None).await
    }

    pub(crate) async fn prepare_update(
        &mut self,
        sql_q: nom_sql::SqlQuery,
        statement_id: u32,
    ) -> std::result::Result<(u64, Vec<Column>), Error> {
        // ensure that we have schemas and endpoints for the query
        let q = if let nom_sql::SqlQuery::Update(ref q) = sql_q {
            q
        } else {
            internal!()
        };

        trace!(table = %q.table.name, "update::access mutator");
        let mutator = self.inner.ensure_mutator(&q.table.name).await?;
        trace!("update::extract schema");
        let schema = Schema::Table(mutator.schema().unwrap().clone());

        // extract parameter columns
        let params: Vec<msql_srv::Column> = {
            utils::get_parameter_columns(&sql_q)
                .into_iter()
                .map(|c| schema_for_column(&schema, c))
                .collect()
        };

        // must have an update query
        let q = if let nom_sql::SqlQuery::Update(q) = sql_q {
            q
        } else {
            internal!();
        };

        trace!(id = statement_id, "update::registered");
        self.prepared_statement_cache
            .insert(statement_id, PreparedStatement::Update(q));
        Ok((statement_id as u64, params))
    }

    pub(crate) async fn execute_prepared_update(
        &mut self,
        q_id: u32,
        params: Vec<DataType>,
    ) -> std::result::Result<(u64, u64), Error> {
        let prep: PreparedStatement = self
            .prepared_statement_cache
            .get(&q_id)
            .ok_or(PreparedStatementMissing)?
            .clone();

        trace!("delegate");
        match prep {
            PreparedStatement::Update(q) => {
                return self.do_update(Cow::Owned(q), Some(params)).await
            }
            _ => internal!(),
        };
    }

    pub(crate) async fn handle_create_table(
        &mut self,
        q: nom_sql::CreateTableStatement,
    ) -> std::result::Result<(), Error> {
        // TODO(malte): we should perhaps check our usual caches here, rather than just blindly
        // doing a migration on Noria ever time. On the other hand, CREATE TABLE is rare...
        info!(table = %q.table.name, "table::create");
        noria_await!(
            self.inner,
            self.inner.noria.extend_recipe(&format!("{};", q))
        )?;
        trace!("table::created");
        Ok(())
    }
}

impl NoriaConnector {
    async fn get_or_create_view(
        &mut self,
        q: &nom_sql::SelectStatement,
        prepared: bool,
    ) -> std::result::Result<String, Error> {
        let qname = match self.tl_cached.get(q) {
            None => {
                // check global cache
                let qname_opt = {
                    let gc = tokio::task::block_in_place(|| self.cached.read().unwrap());
                    gc.get(q).cloned()
                };
                let qname = match qname_opt {
                    Some(qname) => qname,
                    None => {
                        let qh = utils::hash_select_query(q);
                        let qname = format!("q_{:x}", qh);

                        // add the query to Noria
                        if prepared {
                            info!(query = %q, name = %qname, "adding parameterized query");
                        } else {
                            info!(query = %q, name = %qname, "adding ad-hoc query");
                        }
                        if let Err(e) = noria_await!(
                            self.inner,
                            self.inner
                                .noria
                                .extend_recipe(&format!("QUERY {}: {};", qname, q))
                        ) {
                            error!(error = %e, "add query failed");
                            Err(e)?
                        }

                        let mut gc = tokio::task::block_in_place(|| self.cached.write().unwrap());
                        gc.insert(q.clone(), qname.clone());
                        qname
                    }
                };

                self.tl_cached.insert(q.clone(), qname.clone());

                qname
            }
            Some(qname) => qname.to_owned(),
        };
        Ok(qname)
    }

    async fn do_insert(
        &mut self,
        q: &InsertStatement,
        data: Vec<Vec<DataType>>,
    ) -> std::result::Result<(u64, u64), Error> {
        let table = &q.table.name;

        // create a mutator if we don't have one for this table already
        trace!(%table, "insert::access mutator");
        let putter = self.inner.ensure_mutator(table).await?;
        trace!("insert::extract schema");
        let schema = putter
            .schema()
            .ok_or_else(|| internal_err(format!("no schema for table '{}'", table)))?;

        let columns_specified: Vec<_> = q
            .fields
            .as_ref()
            .unwrap()
            .iter()
            .cloned()
            .map(|mut c| {
                c.table = Some(q.table.name.clone());
                c
            })
            .collect();

        // handle auto increment
        trace!("insert::auto-increment");
        let auto_increment_columns: Vec<_> = schema
            .fields
            .iter()
            .filter(|c| c.constraints.contains(&ColumnConstraint::AutoIncrement))
            .collect();
        if auto_increment_columns.len() > 1 {
            // can only have zero or one AUTO_INCREMENT columns
            Err(table_err(table, ReadySetError::MultipleAutoIncrement))?
        }

        let ai = &mut self.auto_increments;
        tokio::task::block_in_place(|| {
            let ai_lock = ai.read().unwrap();
            if ai_lock.get(table).is_none() {
                drop(ai_lock);
                ai.write()
                    .unwrap()
                    .entry(table.to_owned())
                    .or_insert(atomic::AtomicUsize::new(0));
            }
        });
        let mut buf = vec![vec![DataType::None; schema.fields.len()]; data.len()];
        let mut first_inserted_id = None;
        tokio::task::block_in_place(|| -> ReadySetResult<_> {
            let ai_lock = ai.read().unwrap();
            let last_insert_id = &ai_lock[table];

            // handle default values
            trace!("insert::default values");
            let mut default_value_columns: Vec<_> = schema
                .fields
                .iter()
                .filter_map(|ref c| {
                    for cc in &c.constraints {
                        if let ColumnConstraint::DefaultValue(ref v) = *cc {
                            return Some((c.column.clone(), v.clone()));
                        }
                    }
                    None
                })
                .collect();

            trace!("insert::construct ops");

            for (ri, ref row) in data.iter().enumerate() {
                if let Some(col) = auto_increment_columns.iter().next() {
                    let idx = schema
                        .fields
                        .iter()
                        .position(|f| f == *col)
                        .ok_or_else(|| {
                            table_err(table, ReadySetError::NoSuchColumn(col.column.name.clone()))
                        })?;
                    // query can specify an explicit AUTO_INCREMENT value
                    if !columns_specified.contains(&col.column) {
                        let id = last_insert_id.fetch_add(1, atomic::Ordering::SeqCst) as i64 + 1;
                        if first_inserted_id.is_none() {
                            first_inserted_id = Some(id);
                        }
                        buf[ri][idx] = DataType::from(id);
                    }
                }

                for (c, v) in default_value_columns.drain(..) {
                    let idx = schema
                        .fields
                        .iter()
                        .position(|f| f.column == c)
                        .ok_or_else(|| {
                            table_err(table, ReadySetError::NoSuchColumn(c.name.clone()))
                        })?;
                    // only use default value if query doesn't specify one
                    if !columns_specified.contains(&c) {
                        buf[ri][idx] = v.into();
                    }
                }

                for (ci, c) in columns_specified.iter().enumerate() {
                    let (idx, field) = schema
                        .fields
                        .iter()
                        .find_position(|f| f.column == *c)
                        .ok_or_else(|| {
                            table_err(
                                &schema.table.name,
                                ReadySetError::NoSuchColumn(c.name.clone()),
                            )
                        })?;
                    // TODO(grfn): Convert this unwrap() to an actual user error once we have proper
                    // error return values (PR#50)
                    let value = row.get(ci).unwrap().coerce_to(&field.sql_type).unwrap();
                    buf[ri][idx] = value.into_owned();
                }
            }
            Ok(())
        })?;

        let result = if let Some(ref update_fields) = q.on_duplicate {
            trace!("insert::complex");
            invariant_eq!(buf.len(), 1);

            let updates = {
                // fake out an update query
                let mut uq = UpdateStatement {
                    table: nom_sql::Table::from(table.as_str()),
                    fields: update_fields.clone(),
                    where_clause: None,
                };
                utils::extract_update_params_and_fields(
                    &mut uq,
                    &mut None::<std::iter::Empty<DataType>>,
                    schema,
                )?
            };

            // TODO(malte): why can't I consume buf here?
            let r = putter.insert_or_update(buf[0].clone(), updates).await;
            trace!("insert::complex::complete");
            r
        } else {
            trace!("insert::simple");
            let buf: Vec<_> = buf.into_iter().map(TableOperation::Insert).collect();
            let r = putter.perform_all(buf).await;
            trace!("insert::simple::complete");
            r
        };
        result?;
        Ok((data.len() as u64, first_inserted_id.unwrap_or(0) as u64))
    }

    async fn do_read(
        &mut self,
        qname: &str,
        q: &nom_sql::SelectStatement,
        mut keys: Vec<Vec<DataType>>,
        schema: &Vec<Column>,
        key_column_indices: &[usize],
        ticket: Option<Timestamp>,
    ) -> std::result::Result<(Vec<Results>, SelectSchema), Error> {
        // create a getter if we don't have one for this query already
        // TODO(malte): may need to make one anyway if the query has changed w.r.t. an
        // earlier one of the same name
        trace!("select::access view");
        let getter = self
            .inner
            .ensure_getter(&qname, self.region.clone())
            .await?;
        let getter_schema = getter
            .schema()
            .ok_or_else(|| internal_err("No schema for view"))?;
        let mut key_types = key_column_indices
            .iter()
            .map(|i| &getter_schema[*i].sql_type)
            .collect::<Vec<_>>();
        trace!("select::lookup");
        let bogo = vec![vec1![DataType::from(0i32)].into()];
        let cols = Vec::from(getter.columns());
        let mut binops = utils::get_select_statement_binops(q);
        let mut filter_op_idx = None;
        let filter = binops
            .iter()
            .enumerate()
            .filter_map(|(i, (col, binop))| {
                ViewQueryOperator::try_from(*binop)
                    .ok()
                    .map(|op| (i, col, op))
            })
            .next()
            .map(|(idx, col, operator)| -> ReadySetResult<_> {
                let mut key = keys
                    .drain(0..1)
                    .next()
                    .ok_or_else(|| ReadySetError::EmptyKey)?;
                if !keys.is_empty() {
                    unsupported!(
                        "LIKE/ILIKE not currently supported for more than one lookup key at a time"
                    );
                }
                let column = schema
                    .iter()
                    .position(|x| x.column == col.name)
                    .ok_or_else(|| ReadySetError::NoSuchColumn(col.name.clone()))?;
                let value = String::try_from(
                    &key.remove(idx)
                        .coerce_to(&key_types.remove(idx))
                        .unwrap()
                        .into_owned(),
                )?;
                if !key.is_empty() {
                    // the LIKE/ILIKE isn't our only key, add the rest back to `keys`
                    keys.push(key);
                }

                filter_op_idx = Some(idx);

                Ok(ViewQueryFilter {
                    column,
                    operator,
                    value,
                })
            })
            .transpose()?;

        if let Some(filter_op_idx) = filter_op_idx {
            // if we're using a column for a post-lookup filter, remove it from our list of binops
            // so we can use the remaining list for our keys
            binops.remove(filter_op_idx);
        }

        let use_bogo = keys.is_empty();
        let keys = if use_bogo {
            bogo
        } else {
            let mut binops = binops.into_iter().map(|(_, b)| b).unique();
            let binop_to_use = binops.next().unwrap_or(BinaryOperator::Equal);
            if let Some(other) = binops.next() {
                unsupported!("attempted to execute statement with conflicting binary operators {:?} and {:?}", binop_to_use, other);
            }

            keys.drain(..)
                .map(|mut key| {
                    let k = key
                        .drain(..)
                        .zip(&key_types)
                        .map(|(val, col_type)| val.coerce_to(col_type).map(Cow::into_owned))
                        .collect::<ReadySetResult<Vec<DataType>>>()?;

                    Ok((k, binop_to_use)
                        .try_into()
                        .map_err(|_| ReadySetError::EmptyKey)?)
                })
                .collect::<ReadySetResult<Vec<_>>>()?
        };

        let order_by = q
            .order
            .as_ref()
            .map(|oc| -> ReadySetResult<_> {
                // TODO(eta): support this. It isn't necessarily hard, just a pain.
                if oc.columns.len() != 1 {
                    unsupported!(
                        "ORDER BY expressions with more than one column are not supported yet"
                    );
                }
                // TODO(eta): figure out whether this error is actually possible
                let col_idx = schema
                    .iter()
                    .position(|x| x.column == oc.columns[0].0.name)
                    .ok_or_else(|| ReadySetError::NoSuchColumn(oc.columns[0].0.name.clone()))?;
                Ok((
                    col_idx,
                    oc.columns[0].1 == nom_sql::OrderType::OrderDescending,
                ))
            })
            .transpose()?;

        let limit = q
            .limit
            .as_ref()
            .map(|lc| -> ReadySetResult<_> {
                if lc.offset != 0 {
                    unsupported!("OFFSET is not supported yet");
                }
                // FIXME(eta): this cast is ugly!
                Ok(lc.limit as usize)
            })
            .transpose()?;

        let vq = ViewQuery {
            key_comparisons: keys,
            block: true,
            order_by,
            limit,
            filter,
            // TODO(andrew): Add a timestamp to views when RYW consistency
            // is specified.
            timestamp: ticket,
        };

        let data = getter.raw_lookup(vq).await?;
        trace!("select::complete");
        let schema = schema.to_vec();
        Ok((
            data,
            SelectSchema {
                use_bogo,
                schema,
                columns: cols,
            },
        ))
    }

    async fn do_update(
        &mut self,
        q: Cow<'_, UpdateStatement>,
        params: Option<Vec<DataType>>,
    ) -> std::result::Result<(u64, u64), Error> {
        trace!(table = %q.table.name, "update::access mutator");
        let mutator = self.inner.ensure_mutator(&q.table.name).await?;

        let q = q.into_owned();
        let (key, updates) = {
            trace!("update::extract schema");
            let schema = if let Some(cts) = mutator.schema() {
                cts
            } else {
                // no update on views
                unsupported!();
            };
            let coerced_params =
                utils::coerce_params(params, &SqlQuery::Update(q.clone()), &schema)?;
            utils::extract_update(q, coerced_params.map(|p| p.into_iter()), schema)?
        };

        trace!("update::update");
        mutator.update(key, updates).await?;
        trace!("update::complete");
        // TODO: return meaningful fields for (num_rows_updated, last_inserted_id) rather than hardcoded (1,0)
        Ok((1, 0))
    }

    pub(crate) async fn handle_select(
        &mut self,
        q: nom_sql::SelectStatement,
        use_params: Vec<Literal>,
        ticket: Option<Timestamp>,
    ) -> std::result::Result<(Vec<Results>, SelectSchema), Error> {
        trace!("query::select::access view");
        let qname = self.get_or_create_view(&q, false).await?;

        let keys: Vec<Vec<DataType>> = use_params
            .into_iter()
            .map(|l| Ok(vec1![l.to_datatype()?].into()))
            .collect::<Result<Vec<Vec<DataType>>, ReadySetError>>()?;

        // we need the schema for the result writer
        trace!(%qname, "query::select::extract schema");
        let getter_schema = self
            .inner
            .ensure_getter(&qname, self.region.clone())
            .await?
            .schema()
            .ok_or_else(|| internal_err(format!("no schema for view '{}'", qname)))?;

        let schema = schema::convert_schema(&Schema::View(
            getter_schema
                .iter()
                .cloned()
                .filter(|c| c.column.name != "bogokey")
                .collect(),
        ));

        let key_column_indices = utils::select_statement_parameter_columns(&q)
            .into_iter()
            .map(|col| {
                getter_schema
                    .iter()
                    // TODO(grfn): Looking up columns in the resulting view by the name of the
                    // column in the input query is a little iffy - ideally, the getter itself would
                    // be able to tell us the types of the columns and we could skip all of this
                    // nonsense.
                    // https://app.clubhouse.io/readysettech/story/203/add-a-key-types-method-to-view
                    .position(|getter_col| getter_col.column.name == *col.name)
                    .unwrap()
            })
            .collect::<Vec<_>>();

        trace!(%qname, "query::select::do");
        self.do_read(&qname, &q, keys, &schema, &key_column_indices, ticket)
            .await
    }

    pub(crate) async fn prepare_select(
        &mut self,
        mut sql_q: nom_sql::SqlQuery,
        statement_id: u32,
    ) -> std::result::Result<(u32, Vec<msql_srv::Column>, Vec<Column>), Error> {
        // extract parameter columns
        // note that we have to do this *before* collapsing WHERE IN, otherwise the
        // client will be confused about the number of parameters it's supposed to
        // give.
        let param_columns: Vec<nom_sql::Column> = utils::get_parameter_columns(&sql_q)
            .into_iter()
            .cloned()
            .collect();

        trace!("select::collapse where-in clauses");
        let rewritten = rewrite::collapse_where_in(&mut sql_q, false)?;
        let q = if let nom_sql::SqlQuery::Select(q) = sql_q {
            q
        } else {
            internal!();
        };

        // check if we already have this query prepared
        trace!("select::access view");
        let qname = self.get_or_create_view(&q, true).await?;

        // extract result schema
        trace!(qname = %qname, "select::extract schema");
        let getter_schema = self
            .inner
            .ensure_getter(&qname, self.region.clone())
            .await?
            .schema()
            .ok_or_else(|| internal_err(format!("no schema for view '{}'", qname)))?;

        let schema = Schema::View(
            getter_schema
                .iter()
                .cloned()
                .filter(|c| c.column.name != "bogokey")
                .collect(),
        );

        let key_column_indices = param_columns
            .iter()
            .map(|col| {
                getter_schema
                    .iter()
                    // TODO: https://app.clubhouse.io/readysettech/story/203/add-a-key-types-method-to-view
                    .position(|getter_col| getter_col.column.name == *col.name)
                    .unwrap()
            })
            .collect::<Vec<_>>();

        // now convert params to msql_srv types; we have to do this here because we don't have
        // access to the schema yet when we extract them above.
        let params: Vec<msql_srv::Column> = param_columns
            .into_iter()
            .map(|mut c| {
                c.table = Some(qname.clone());
                schema_for_column(&schema, &c)
            })
            .collect();
        let schema = schema::convert_schema(&schema);
        let select_schema = schema.clone();
        trace!(id = statement_id, "select::registered");
        let ps = PreparedStatement::Select {
            name: qname,
            statement: q,
            schema: select_schema,
            key_column_indices,
            rewritten_columns: rewritten.map(|(a, b)| (a, b.len())),
        };
        self.prepared_statement_cache.insert(statement_id, ps);
        Ok((statement_id, params, schema))
    }

    pub(crate) async fn execute_prepared_select(
        &mut self,
        q_id: u32,
        params: Vec<DataType>,
        ticket: Option<Timestamp>,
    ) -> std::result::Result<(Vec<Results>, SelectSchema), Error> {
        let prep: PreparedStatement = {
            match self.prepared_statement_cache.get(&q_id) {
                Some(e) => e.clone(),
                None => Err(PreparedStatementMissing)?,
            }
        };

        match &prep {
            PreparedStatement::Select {
                name,
                statement: q,
                schema,
                key_column_indices,
                rewritten_columns: rewritten,
            } => {
                trace!("apply where-in rewrites");
                let keys = match rewritten {
                    Some((first_rewritten, nrewritten)) => {
                        // this is a little tricky
                        // the user is giving us some params [a, b, c, d]
                        // for the query WHERE x = ? AND y IN (?, ?) AND z = ?
                        // that we rewrote to WHERE x = ? AND y = ? AND z = ?
                        // so we need to turn that into the keys:
                        // [[a, b, d], [a, c, d]]
                        if params.len() == 0 {
                            Err(ReadySetError::EmptyKey)?
                        }
                        (0..*nrewritten)
                            .map(|poffset| {
                                params
                                    .iter()
                                    .take(*first_rewritten)
                                    .chain(params.iter().skip(first_rewritten + poffset).take(1))
                                    .chain(params.iter().skip(first_rewritten + nrewritten))
                                    .cloned()
                                    .collect()
                            })
                            .collect()
                    }
                    None => {
                        if params.len() > 0 {
                            vec![params]
                        } else {
                            vec![]
                        }
                    }
                };

                return self
                    .do_read(name, q, keys, schema, key_column_indices, ticket)
                    .await;
            }
            _ => {
                internal!()
            }
        };
    }

    pub(crate) async fn handle_create_view(
        &mut self,
        q: nom_sql::CreateViewStatement,
    ) -> std::result::Result<(), Error> {
        // TODO(malte): we should perhaps check our usual caches here, rather than just blindly
        // doing a migration on Noria every time. On the other hand, CREATE VIEW is rare...

        info!(%q.definition, %q.name, "view::create");

        noria_await!(
            self.inner,
            self.inner
                .noria
                .extend_recipe(&format!("VIEW {}: {};", q.name, q.definition))
        )?;

        Ok(())
    }
}
