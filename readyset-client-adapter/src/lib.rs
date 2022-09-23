#![deny(macro_use_extern_crate)]

use std::collections::HashMap;
use std::io;
use std::marker::Send;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use clap::Parser;
use failpoint_macros::set_failpoint;
use futures_util::future::FutureExt;
use futures_util::stream::StreamExt;
use launchpad::futures::abort_on_panic;
use launchpad::redacted::RedactedString;
use maplit::hashmap;
use metrics::SharedString;
use metrics_exporter_prometheus::PrometheusBuilder;
use nom_sql::{Dialect, Relation, SqlQuery};
use readyset::consensus::{AuthorityControl, AuthorityType, ConsulAuthority};
#[cfg(feature = "failure_injection")]
use readyset::failpoints;
use readyset::metrics::recorded;
use readyset::{ReadySetError, ReadySetHandle, ViewCreateRequest};
use readyset_client::backend::noria_connector::{NoriaConnector, ReadBehavior};
use readyset_client::backend::MigrationMode;
use readyset_client::health_reporter::AdapterHealthReporter;
use readyset_client::http_router::NoriaAdapterHttpRouter;
use readyset_client::migration_handler::MigrationHandler;
use readyset_client::query_status_cache::{MigrationStyle, QueryStatusCache};
use readyset_client::views_synchronizer::ViewsSynchronizer;
use readyset_client::{Backend, BackendBuilder, QueryHandler, UpstreamDatabase};
use readyset_client_metrics::QueryExecutionEvent;
use readyset_dataflow::Readers;
use readyset_server::metrics::{CompositeMetricsRecorder, MetricsRecorder};
use readyset_server::worker::readers::{retry_misses, Ack, BlockingRead, ReadRequestHandler};
use readyset_sql_passes::anonymize::anonymize_literals;
use readyset_telemetry_reporter::{TelemetryBuilder, TelemetryEvent, TelemetryInitializer};
use readyset_version::COMMIT_ID;
use stream_cancel::Valve;
use tokio::net::UdpSocket;
use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;
use tokio::{net, select};
use tokio_stream::wrappers::TcpListenerStream;
use tracing::{debug, debug_span, error, info, info_span, span, warn, Level};
use tracing_futures::Instrument;

// How frequently to try to establish an http registration for the first time or if the last tick
// failed and we need to establish a new one
const REGISTER_HTTP_INIT_INTERVAL: Duration = Duration::from_secs(2);

// How frequently to try to establish an http registration if we have one already
const REGISTER_HTTP_INTERVAL: Duration = Duration::from_secs(20);

const AWS_PRIVATE_IP_ENDPOINT: &str = "http://169.254.169.254/latest/meta-data/local-ipv4";
const AWS_METADATA_TOKEN_ENDPOINT: &str = "http://169.254.169.254/latest/api/token";

/// Timeout to use when connecting to the upstream database
const UPSTREAM_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

#[async_trait]
pub trait ConnectionHandler {
    type UpstreamDatabase: UpstreamDatabase;
    type Handler: QueryHandler;

    async fn process_connection(
        &mut self,
        stream: net::TcpStream,
        backend: Backend<Self::UpstreamDatabase, Self::Handler>,
    );

    /// Return an immediate error to a newly-established connection, then immediately disconnect
    async fn immediate_error(self, stream: net::TcpStream, error_message: String);
}

/// Represents which database interface is being adapted to communicate with ReadySet.
#[derive(Copy, Clone, Debug)]
pub enum DatabaseType {
    /// MySQL database.
    Mysql,
    /// PostgreSQL database.
    Psql,
}

/// How to behave when receiving unsupported `SET` statements.
///
/// Corresponds to the variants of [`noria_client::backend::UnsupportedSetMode`] that are exposed to
/// the user.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UnsupportedSetMode {
    /// Return an error to the client (the default)
    Error,
    /// Proxy all subsequent statements to the upstream
    Proxy,
}

impl Default for UnsupportedSetMode {
    fn default() -> Self {
        Self::Error
    }
}

impl FromStr for UnsupportedSetMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "error" => Ok(Self::Error),
            "proxy" => Ok(Self::Proxy),
            _ => bail!(
                "Invalid value for unsupoported_set_mode; expected one of \"error\" or \"proxy\""
            ),
        }
    }
}

impl From<UnsupportedSetMode> for readyset_client::backend::UnsupportedSetMode {
    fn from(mode: UnsupportedSetMode) -> Self {
        match mode {
            UnsupportedSetMode::Error => Self::Error,
            UnsupportedSetMode::Proxy => Self::Proxy,
        }
    }
}

pub struct NoriaAdapter<H>
where
    H: ConnectionHandler,
{
    pub description: &'static str,
    pub default_address: SocketAddr,
    pub connection_handler: H,
    pub database_type: DatabaseType,
    /// SQL dialect to use when parsing queries
    pub dialect: Dialect,
    /// Configuration for the connection handler's upstream database
    pub upstream_config: <<H as ConnectionHandler>::UpstreamDatabase as UpstreamDatabase>::Config,
}

#[derive(Parser, Debug)]
pub struct Options {
    /// IP:PORT to listen on
    #[clap(long, short = 'a', env = "LISTEN_ADDRESS", parse(try_from_str))]
    address: Option<SocketAddr>,

    /// ReadySet deployment ID to attach to
    #[clap(long, env = "NORIA_DEPLOYMENT", forbid_empty_values = true)]
    deployment: String,

    /// The authority to use. Possible values: zookeeper, consul, standalone.
    #[clap(
        long,
        env = "AUTHORITY",
        default_value_if("standalone", None, Some("standalone")),
        default_value = "consul",
        possible_values = &["consul", "zookeeper", "standalone"]
    )]
    authority: AuthorityType,

    /// Authority uri
    // NOTE: `authority_address` should come after `authority` for clap to set default values
    // properly
    #[clap(
        long,
        env = "AUTHORITY_ADDRESS",
        default_value_if("authority", Some("standalone"), Some(".")),
        default_value_if("authority", Some("consul"), Some("127.0.0.1:8500")),
        default_value_if("authority", Some("zookeeper"), Some("127.0.0.1:2181"))
    )]
    authority_address: String,

    /// Log slow queries (> 5ms)
    #[clap(long)]
    log_slow: bool,

    /// Don't require authentication for any client connections
    #[clap(long, env = "ALLOW_UNAUTHENTICATED_CONNECTIONS")]
    allow_unauthenticated_connections: bool,

    /// Run migrations in a separate thread off of the serving path.
    #[clap(long, env = "ASYNC_MIGRATIONS", requires("upstream-db-url"))]
    async_migrations: bool,

    /// Sets the maximum time in minutes that we will retry migrations for in the
    /// migration handler. If this time is reached, the query will be exclusively
    /// sent to fallback.
    ///
    /// Defaults to 15 minutes.
    #[clap(long, env = "MAX_PROCESSING_MINUTES", default_value = "15")]
    max_processing_minutes: u64,

    /// Sets the migration handlers's loop interval in milliseconds.
    #[clap(long, env = "MIGRATION_TASK_INTERVAL", default_value = "20000")]
    migration_task_interval: u64,

    /// Validate queries executing against noria with the upstream db.
    #[clap(long, env = "VALIDATE_QUERIES", requires("upstream-db-url"))]
    validate_queries: bool,

    /// IP:PORT to host endpoint for scraping metrics from the adapter.
    #[clap(
        long,
        env = "METRICS_ADDRESS",
        default_value = "0.0.0.0:6034",
        parse(try_from_str)
    )]
    metrics_address: SocketAddr,

    /// Allow database connections authenticated as this user. Ignored if
    /// --allow-unauthenticated-connections is passed
    #[clap(long, env = "ALLOWED_USERNAME", short = 'u')]
    username: Option<String>,

    /// Password to authenticate database connections with. Ignored if
    /// --allow-unauthenticated-connections is passed
    #[clap(long, env = "ALLOWED_PASSWORD", short = 'p')]
    password: Option<RedactedString>,

    /// URL for the upstream database to connect to. Should include username and password if
    /// necessary
    #[clap(long, env = "UPSTREAM_DB_URL")]
    upstream_db_url: Option<RedactedString>,

    /// Enable recording and exposing Prometheus metrics
    #[clap(long, env = "PROMETHEUS_METRICS")]
    prometheus_metrics: bool,

    #[clap(long, hide = true)]
    noria_metrics: bool,

    /// Enable logging queries and execution metrics in prometheus. This creates a
    /// histogram per unique query.
    #[clap(long, env = "QUERY_LOG", requires = "prometheus-metrics")]
    query_log: bool,

    /// Enables logging ad-hoc queries in the query log. Useful for testing.
    #[clap(long, hide = true, env = "QUERY_LOG_AD_HOC", requires = "query-log")]
    query_log_ad_hoc: bool,

    /// Use the AWS EC2 metadata service to determine the external address of this noria adapter's
    /// http endpoint.
    #[clap(long)]
    use_aws_external_address: bool,

    #[clap(flatten)]
    tracing: readyset_tracing::Options,

    /// Test feature to fail invalidated queries in the serving path instead of going
    /// to fallback.
    #[clap(long, hide = true)]
    fail_invalidated_queries: bool,

    /// Allow executing, but ignore, unsupported `SET` statements.
    ///
    /// Takes precedence over any value passed to `--unsupported-set-mode`
    #[clap(long, hide = true, env = "ALLOW_UNSUPPORTED_SET")]
    allow_unsupported_set: bool,

    /// Configure how ReadySet behaves when receiving unsupported SET statements.
    ///
    /// The possible values are:
    ///
    /// * "error" (default) - return an error to the client
    /// * "proxy" - proxy all subsequent statements
    // NOTE: In order to keep `allow_unsupported_set` hidden, we're keeping these two flags separate
    // and *not* marking them as conflicting with each other.
    #[clap(
        long,
        env = "UNSUPPORTED_SET_MODE",
        default_value = "error",
        possible_values = &["error", "proxy"],
        parse(try_from_str)
    )]
    unsupported_set_mode: UnsupportedSetMode,

    /// Only run migrations through CREATE CACHE statements. Async migrations are not
    /// supported in this case.
    #[clap(long, env = "EXPLICIT_MIGRATIONS", conflicts_with = "async-migrations")]
    explicit_migrations: bool,

    // TODO(DAN): require explicit migrations
    /// Specifies the polling interval in seconds for requesting views from the Leader.
    #[clap(long, env = "OUTPUTS_POLLING_INTERVAL", default_value = "300")]
    views_polling_interval: u64,

    /// The time to wait before canceling a migration request. Defaults to 30 minutes.
    #[clap(
        long,
        hide = true,
        env = "MIGRATION_REQUEST_TIMEOUT",
        default_value = "1800000"
    )]
    migration_request_timeout_ms: u64,

    /// The time to wait before canceling a controller request. Defaults to 5 seconds.
    #[clap(long, hide = true, env = "CONTROLLER_TIMEOUT", default_value = "5000")]
    controller_request_timeout_ms: u64,

    /// Specifies the maximum continuous failure time for any given query, in seconds, before
    /// entering into a fallback recovery mode.
    #[clap(
        long,
        hide = true,
        env = "QUERY_MAX_FAILURE_SECONDS",
        default_value = "9223372036854775"
    )]
    query_max_failure_seconds: u64,

    /// Specifies the recovery period in seconds that we enter if a given query fails for the
    /// period of time designated by the query_max_failure_seconds flag.
    #[clap(
        long,
        hide = true,
        env = "FALLBACK_RECOVERY_SECONDS",
        default_value = "0"
    )]
    fallback_recovery_seconds: u64,

    /// Whether to use non-blocking or blocking reads against the cache.
    #[clap(long, env = "NON_BLOCKING_READS")]
    non_blocking_reads: bool,

    /// Run ReadySet in standalone mode, running a readyset-server and readyset-mysql instance
    /// within this adapter.
    #[clap(long, env = "STANDALONE", conflicts_with = "embedded-readers")]
    standalone: bool,

    /// Run ReadySet in embedded readers mode, running reader replicas (and only reader replicas)
    /// in the same process as the adapter
    ///
    /// Should be combined with passing `--no-readers` and `--reader-replicas` with the number of
    /// adapter instances to each server process.
    #[clap(long, env = "EMBEDDED_READERS", conflicts_with = "standalone")]
    embedded_readers: bool,

    #[clap(flatten)]
    server_worker_options: readyset_server::WorkerOptions,

    /// Whether to disable telemetry reporting. Defaults to false.
    #[clap(long, env = "DISABLE_TELEMETRY")]
    disable_telemetry: bool,
}

impl<H> NoriaAdapter<H>
where
    H: ConnectionHandler + Clone + Send + Sync + 'static,
{
    pub fn run(&mut self, options: Options) -> anyhow::Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async { options.tracing.init("adapter") })?;
        info!(?options, "Starting ReadySet adapter");
        let users: &'static HashMap<String, String> = Box::leak(Box::new(
            if !options.allow_unauthenticated_connections {
                hashmap! {
                    options.username.ok_or_else(|| {
                        anyhow!("Must specify --username/-u unless --allow-unauthenticated-connections is passed")
                    })? => options.password.map(|x| x.0).ok_or_else(|| {
                        anyhow!("Must specify --password/-p unless --allow-unauthenticated-connections is passed")
                    })?
                }
            } else {
                HashMap::new()
            },
        ));
        info!(commit_hash = %COMMIT_ID);

        let telemetry_sender = rt.block_on(async {
            TelemetryInitializer::init(options.disable_telemetry, std::env::var("RS_API_KEY").ok())
                .await
        });

        let _ = rt
            .block_on(async {
                telemetry_sender
                    .send_event_with_payload(
                        TelemetryEvent::AdapterStart,
                        TelemetryBuilder::new()
                            .adapter_version(option_env!("CARGO_PKG_VERSION").unwrap_or_default())
                            .db_backend(format!("{:?}", &self.database_type).to_lowercase())
                            .build(),
                    )
                    .await
            })
            .map_err(|error| warn!(%error, "Failed to initialize telemetry sender"));

        if options.allow_unsupported_set {
            warn!(
                "Running with --allow-unsupported-set can cause certain queries to return \
                 incorrect results"
            )
        }

        let listen_address = options.address.unwrap_or(self.default_address);
        let listener = rt.block_on(tokio::net::TcpListener::bind(&listen_address))?;

        info!(%listen_address, "Listening for new connections");

        let auto_increments: Arc<RwLock<HashMap<Relation, AtomicUsize>>> = Arc::default();
        let query_cache: Arc<RwLock<HashMap<ViewCreateRequest, Relation>>> = Arc::default();
        let health_reporter = AdapterHealthReporter::new();

        let rs_connect = span!(Level::INFO, "Connecting to RS server");
        rs_connect.in_scope(|| info!(%options.authority_address, %options.deployment));

        let authority = options.authority.clone();
        let authority_address = options.authority_address.clone();
        let deployment = options.deployment.clone();
        let migration_request_timeout = options.migration_request_timeout_ms;
        let controller_request_timeout = options.controller_request_timeout_ms;
        let rh = rt.block_on(async {
            let authority = authority
                .to_authority(&authority_address, &deployment)
                .await;

            Ok::<ReadySetHandle, ReadySetError>(
                ReadySetHandle::with_timeouts(
                    authority,
                    Some(Duration::from_millis(controller_request_timeout)),
                    Some(Duration::from_millis(migration_request_timeout)),
                )
                .instrument(rs_connect.clone())
                .await,
            )
        })?;

        rs_connect.in_scope(|| info!("ReadySetHandle created"));

        let ctrlc = tokio::signal::ctrl_c();
        let mut sigterm = {
            let _guard = rt.enter();
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap()
        };
        let mut listener = Box::pin(futures_util::stream::select(
            TcpListenerStream::new(listener),
            futures_util::stream::select(
                ctrlc
                    .map(|r| {
                        r?;
                        Err(io::Error::new(io::ErrorKind::Interrupted, "got ctrl-c"))
                    })
                    .into_stream(),
                sigterm
                    .recv()
                    .map(futures_util::stream::iter)
                    .into_stream()
                    .flatten()
                    .map(|_| Err(io::Error::new(io::ErrorKind::Interrupted, "got SIGTERM"))),
            ),
        ));
        rs_connect.in_scope(|| info!("Now capturing ctrl-c and SIGTERM events"));

        let mut recorders = Vec::new();
        let prometheus_handle = if options.prometheus_metrics {
            let _guard = rt.enter();
            let database_label: readyset_client_metrics::DatabaseType = self.database_type.into();

            let recorder = PrometheusBuilder::new()
                .add_global_label("upstream_db_type", database_label)
                .add_global_label("deployment", &options.deployment)
                .build_recorder();

            let handle = recorder.handle();
            recorders.push(MetricsRecorder::Prometheus(recorder));
            Some(handle)
        } else {
            None
        };

        if options.noria_metrics {
            recorders.push(MetricsRecorder::Noria(
                readyset_server::NoriaMetricsRecorder::new(),
            ));
        }

        if !recorders.is_empty() {
            readyset_server::metrics::install_global_recorder(
                CompositeMetricsRecorder::with_recorders(recorders),
            )?;
        }

        rs_connect.in_scope(|| info!("PrometheusHandle created"));

        metrics::counter!(
            recorded::NORIA_STARTUP_TIMESTAMP,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
        );

        let (shutdown_sender, shutdown_recv) = tokio::sync::broadcast::channel(1);

        // Gate query log code path on the log flag existing.
        let qlog_sender = if options.query_log {
            rs_connect.in_scope(|| info!("Query logs are enabled. Spawning query logger"));
            let (qlog_sender, qlog_receiver) = tokio::sync::mpsc::unbounded_channel();
            rt.spawn(query_logger(qlog_receiver, shutdown_recv));
            Some(qlog_sender)
        } else {
            rs_connect.in_scope(|| info!("Query logs are disabled"));
            None
        };

        let noria_read_behavior = if options.non_blocking_reads {
            rs_connect.in_scope(|| info!("Will perform NonBlocking Reads"));
            ReadBehavior::NonBlocking
        } else {
            rs_connect.in_scope(|| info!("Will perform Blocking Reads"));
            ReadBehavior::Blocking
        };

        let migration_style = if options.async_migrations {
            MigrationStyle::Async
        } else if options.explicit_migrations {
            MigrationStyle::Explicit
        } else {
            MigrationStyle::InRequestPath
        };
        rs_connect.in_scope(|| info!(?migration_style));

        let query_status_cache: &'static _ =
            Box::leak(Box::new(QueryStatusCache::with_style(migration_style)));

        let migration_mode = if options.async_migrations || options.explicit_migrations {
            MigrationMode::OutOfBand
        } else {
            MigrationMode::InRequestPath
        };
        rs_connect.in_scope(|| info!(?migration_mode));

        if let MigrationMode::OutOfBand = migration_mode {
            let upstream_db_url = options.upstream_db_url.as_ref().map(|u| u.0.clone());
            let upstream_config = self.upstream_config.clone();
            let rh = rh.clone();
            let (auto_increments, query_cache) = (auto_increments.clone(), query_cache.clone());
            let shutdown_recv = shutdown_sender.subscribe();
            let loop_interval = options.migration_task_interval;
            let max_retry = options.max_processing_minutes;
            let validate_queries = options.validate_queries;
            let dry_run = options.explicit_migrations;

            rs_connect.in_scope(|| info!("Spawning migration handler task"));
            let fut = async move {
                let connection = span!(Level::INFO, "migration task upstream database connection");
                let mut upstream =
                    match upstream_db_url {
                        Some(url) if !dry_run => Some(
                            H::UpstreamDatabase::connect(url.clone(), upstream_config)
                                .instrument(connection.in_scope(|| {
                                    span!(Level::INFO, "Connecting to upstream database")
                                }))
                                .await
                                .unwrap(),
                        ),
                        _ => None,
                    };

                let schema_search_path = if let Some(upstream) = &mut upstream {
                    // TODO(ENG-1710): figure out a better error handling story for this task
                    upstream.schema_search_path().await.unwrap()
                } else {
                    Default::default()
                };

                //TODO(DAN): allow compatibility with async and explicit migrations
                let noria =
                    NoriaConnector::new(
                        rh.clone(),
                        auto_increments.clone(),
                        query_cache.clone(),
                        noria_read_behavior,
                        schema_search_path,
                    )
                    .instrument(connection.in_scope(|| {
                        span!(Level::DEBUG, "Building migration task noria connector")
                    }))
                    .await;

                let controller_handle = dry_run.then(|| rh.clone());
                let mut migration_handler = MigrationHandler::new(
                    noria,
                    upstream,
                    controller_handle,
                    query_status_cache,
                    validate_queries,
                    std::time::Duration::from_millis(loop_interval),
                    std::time::Duration::from_secs(max_retry * 60),
                    shutdown_recv,
                );

                migration_handler.run().await.map_err(move |e| {
                    error!(error = %e, "Migration Handler failed, aborting the process due to service entering a degraded state");
                    std::process::abort()
                })
            };

            rt.handle().spawn(abort_on_panic(fut));
        }

        if options.explicit_migrations {
            rs_connect.in_scope(|| info!("Spawning explicit migrations task"));
            let rh = rh.clone();
            let loop_interval = options.views_polling_interval;
            let shutdown_recv = shutdown_sender.subscribe();
            let fut = async move {
                let mut views_synchronizer = ViewsSynchronizer::new(
                    rh,
                    query_status_cache,
                    std::time::Duration::from_secs(loop_interval),
                    shutdown_recv,
                );
                views_synchronizer.run().await
            };
            rt.handle().spawn(abort_on_panic(fut));
        }

        // Spin up async task that is in charge of creating a session with the authority,
        // regularly updating the heartbeat to keep the session live, and registering the adapters
        // http endpoint.
        // For now we only support registering adapters over consul.
        if let AuthorityType::Consul = options.authority {
            rs_connect.in_scope(|| info!("Spawning Consul session task"));
            let connection = span!(Level::DEBUG, "consul_session", addr = ?authority_address);
            let fut = reconcile_endpoint_registration(
                authority_address,
                deployment,
                options.metrics_address.port(),
                options.use_aws_external_address,
            )
            .instrument(connection);
            rt.handle().spawn(fut);
        }

        // Create a set of readers on this adapter. This will allow servicing queries directly
        // from readers on the adapter rather than across a network hop.
        let readers: Readers = Arc::new(Mutex::new(Default::default()));

        // Run a readyset-server instance within this adapter.
        let _handle = if options.standalone || options.embedded_readers {
            let (handle, valve) = Valve::new();
            let authority = options.authority.clone();
            let deployment = options.deployment.clone();
            let mut builder = readyset_server::Builder::from_worker_options(
                options.server_worker_options,
                &options.deployment,
            );
            let r = readers.clone();
            let auth_address = options.authority_address.clone();

            if options.embedded_readers {
                builder.as_reader_only();
                builder.cannot_become_leader();
            }

            if let Some(upstream_db_url) = &options.upstream_db_url {
                builder.set_replication_url(upstream_db_url.clone().into());
            }

            builder.set_telemetry_sender(telemetry_sender.clone());

            let server_handle = rt.block_on(async move {
                let authority = Arc::new(authority.to_authority(&auth_address, &deployment).await);

                builder
                    .start_with_readers(
                        authority,
                        r,
                        SocketAddr::new(
                            std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
                            4000,
                        ),
                        valve,
                        handle,
                    )
                    .await
            })?;

            Some(server_handle)
        } else {
            None
        };

        // Spawn a task for handling this adapter's HTTP request server.
        // This step is done as the last thing before accepting connections because it is used as
        // the health check for the service.
        let router_handle = {
            rs_connect.in_scope(|| info!("Spawning HTTP request server task"));
            let (handle, valve) = Valve::new();
            let http_server = NoriaAdapterHttpRouter {
                listen_addr: options.metrics_address,
                query_cache: query_status_cache,
                valve,
                prometheus_handle,
                health_reporter,
            };

            let fut = async move {
                let http_listener = http_server.create_listener().await.unwrap();
                NoriaAdapterHttpRouter::route_requests(http_server, http_listener).await
            };

            rt.handle().spawn(fut);

            handle
        };

        while let Some(Ok(s)) = rt.block_on(listener.next()) {
            let connection = span!(Level::DEBUG, "connection", addr = ?s.peer_addr().unwrap());
            connection.in_scope(|| info!("Accepted new connection"));

            // bunch of stuff to move into the async block below
            let rh = rh.clone();
            let (auto_increments, query_cache) = (auto_increments.clone(), query_cache.clone());
            let mut connection_handler = self.connection_handler.clone();
            let upstream_db_url = options.upstream_db_url.clone();
            let upstream_config = self.upstream_config.clone();
            let backend_builder = BackendBuilder::new()
                .slowlog(options.log_slow)
                .users(users.clone())
                .require_authentication(!options.allow_unauthenticated_connections)
                .dialect(self.dialect)
                .query_log(qlog_sender.clone(), options.query_log_ad_hoc)
                .validate_queries(options.validate_queries, options.fail_invalidated_queries)
                .unsupported_set_mode(if options.allow_unsupported_set {
                    readyset_client::backend::UnsupportedSetMode::Allow
                } else {
                    options.unsupported_set_mode.into()
                })
                .migration_mode(migration_mode)
                .query_max_failure_seconds(options.query_max_failure_seconds)
                .telemetry_sender(telemetry_sender.clone())
                .fallback_recovery_seconds(options.fallback_recovery_seconds);
            let telemetry_sender = telemetry_sender.clone();

            // Initialize the reader layer for the adapter.
            let r = (options.standalone || options.embedded_readers).then(|| {
                // Create a task that repeatedly polls BlockingRead's every `RETRY_TIMEOUT`.
                // When the `BlockingRead` completes, tell the future to resolve with ack.
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<(BlockingRead, Ack)>();
                rt.handle().spawn(retry_misses(rx));
                ReadRequestHandler::new(readers.clone(), tx, Duration::from_secs(5))
            });

            let query_status_cache = query_status_cache;
            let fut = async move {
                let upstream_res = if let Some(upstream_db_url) = &upstream_db_url {
                    set_failpoint!(failpoints::UPSTREAM);
                    timeout(
                        UPSTREAM_CONNECTION_TIMEOUT,
                        H::UpstreamDatabase::connect(
                            upstream_db_url.0.clone(),
                            upstream_config.clone(),
                        ),
                    )
                    .instrument(debug_span!("Connecting to upstream database"))
                    .await
                    .map_err(|_| "Connection timed out".to_owned())
                    .and_then(|r| r.map_err(|e| e.to_string()))
                    .map_err(|e| format!("Error connecting to upstream database: {}", e))
                    .map(Some)
                } else {
                    Ok(None)
                };

                match upstream_res {
                    Ok(mut upstream) => {
                        if let Err(e) = telemetry_sender
                            .send_event(TelemetryEvent::UpstreamConnected)
                            .await
                        {
                            warn!(error = %e, "Failed to send upstream connected metric");
                        }

                        // Query the upstream for its currently-configured schema search path
                        //
                        // NOTE: when we start tracking all configuration parameters, this should be
                        // folded into whatever loads those initially
                        let schema_search_path_res = if let Some(upstream) = &mut upstream {
                            upstream.schema_search_path().await.map(|ssp| {
                                debug!(
                                    schema_search_path = ?ssp,
                                    "Setting initial schema search path for backend"
                                );
                                ssp
                            })
                        } else {
                            Ok(Default::default())
                        };

                        match schema_search_path_res {
                            Ok(ssp) => {
                                let noria = NoriaConnector::new_with_local_reads(
                                    rh.clone(),
                                    auto_increments.clone(),
                                    query_cache.clone(),
                                    noria_read_behavior,
                                    r,
                                    ssp,
                                )
                                .instrument(debug_span!("Building noria connector"))
                                .await;

                                let backend = backend_builder.clone().build(
                                    noria,
                                    upstream,
                                    query_status_cache,
                                );
                                connection_handler.process_connection(s, backend).await;
                            }
                            Err(error) => {
                                error!(
                                    %error,
                                    "Error loading initial schema search path from upstream"
                                );
                                connection_handler
                                    .immediate_error(
                                        s,
                                        format!(
                                            "Error loading initial schema search path from \
                                             upstream: {error}"
                                        ),
                                    )
                                    .await;
                            }
                        }
                    }
                    Err(error) => {
                        error!(%error, "Error during initial connection establishment");
                        connection_handler.immediate_error(s, error).await;
                    }
                }

                debug!("disconnected");
            }
            .instrument(connection);

            rt.handle().spawn(fut);
        }

        let rs_shutdown = span!(Level::INFO, "RS server Shutting down");
        // Dropping the sender acts as a shutdown signal.
        drop(shutdown_sender);

        rs_shutdown.in_scope(|| {
            info!("Shutting down all tcp streams started by the adapters http router")
        });
        drop(router_handle);

        rs_shutdown.in_scope(|| info!("Dropping controller handle"));
        drop(rh);

        // Send shutdown telemetry events
        if _handle.is_some() {
            let _ = rt.block_on(telemetry_sender.send_event(TelemetryEvent::ServerStop));
        }

        let _ = rt.block_on(telemetry_sender.send_event(TelemetryEvent::AdapterStop));

        // We use `shutdown_timeout` instead of `shutdown_background` in case any
        // blocking IO is ongoing.
        rs_shutdown.in_scope(|| info!("Waiting up to 20s for tasks to complete shutdown"));
        rt.shutdown_timeout(std::time::Duration::from_secs(20));
        rs_shutdown.in_scope(|| info!("Shutdown completed successfully"));

        Ok(())
    }
}

async fn my_ip(destination: &str, use_aws_external: bool) -> Option<IpAddr> {
    if use_aws_external {
        return my_aws_ip().await.ok();
    }

    let socket = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(_) => return None,
    };

    match socket.connect(destination).await {
        Ok(()) => (),
        Err(_) => return None,
    };

    match socket.local_addr() {
        Ok(addr) => Some(addr.ip()),
        Err(_) => None,
    }
}

// TODO(peter): Pull this out to a shared util between readyset-server and readyset-adapter
async fn my_aws_ip() -> anyhow::Result<IpAddr> {
    let client = reqwest::Client::builder().build()?;
    let token: String = client
        .put(AWS_METADATA_TOKEN_ENDPOINT)
        .header("X-aws-ec2-metadata-token-ttl-seconds", "21600")
        .send()
        .await?
        .text()
        .await?
        .parse()?;

    Ok(client
        .get(AWS_PRIVATE_IP_ENDPOINT)
        .header("X-aws-ec2-metadata-token", &token)
        .send()
        .await?
        .text()
        .await?
        .parse()?)
}

/// Facilitates continuously updating consul with this adapters externally accessibly http
/// endpoint.
async fn reconcile_endpoint_registration(
    authority_address: String,
    deployment: String,
    port: u16,
    use_aws_external: bool,
) {
    let connect_string = format!("http://{}/{}", &authority_address, &deployment);
    debug!("{}", connect_string);
    let authority = ConsulAuthority::new(&connect_string).unwrap();

    let mut initializing = true;
    let mut interval = tokio::time::interval(REGISTER_HTTP_INIT_INTERVAL);
    let mut session_id = None;

    async fn needs_refresh(id: &Option<String>, consul: &ConsulAuthority) -> bool {
        if let Some(id) = id {
            consul.worker_heartbeat(id.to_owned()).await.is_err()
        } else {
            true
        }
    }

    loop {
        interval.tick().await;
        debug!("Checking authority registry");

        if needs_refresh(&session_id, &authority).await {
            // If we fail this heartbeat, we assume we need to create a new session.
            if let Err(e) = authority.init().await {
                error!(%e, "encountered error while trying to initialize authority in readyset-adapter");
                // Try again on next tick, and reduce the polling interval until a new session is
                // established.
                initializing = true;
                continue;
            }
        }

        // We try to update our http endpoint every iteration regardless because it may
        // have changed.
        let ip = match my_ip(&authority_address, use_aws_external).await {
            Some(ip) => ip,
            None => {
                info!("Failed to retrieve IP. Will try again on next tick");
                continue;
            }
        };
        let http_endpoint = SocketAddr::new(ip, port);

        match authority.register_adapter(http_endpoint).await {
            Ok(id) => {
                if initializing {
                    info!("Extablished authority connection, reducing polling interval");
                    // Switch to a longer polling interval after the first registration is made
                    interval = tokio::time::interval(REGISTER_HTTP_INTERVAL);
                    initializing = false;
                }

                session_id = id;
            }
            Err(e) => {
                error!(%e, "encountered error while trying to register adapter endpoint in authority")
            }
        }
    }
}

/// Async task that logs query stats.
async fn query_logger(
    mut receiver: UnboundedReceiver<QueryExecutionEvent>,
    mut shutdown_recv: broadcast::Receiver<()>,
) {
    let _span = info_span!("query-logger");

    loop {
        select! {
            event = receiver.recv() => {
                if let Some(event) = event {
                    let query = match event.query {
                        Some(s) => match s.as_ref() {
                            SqlQuery::Select(stmt) => {
                                let mut stmt = stmt.clone();
                                if readyset_client::rewrite::process_query(&mut stmt, true).is_ok() {
                                    anonymize_literals(&mut stmt);
                                    stmt.to_string()
                                } else {
                                    "".to_string()
                                }
                            },
                            _ => "".to_string()
                        },
                        _ => "".to_string()
                    };

                    if let Some(num_keys) = event.num_keys {
                        metrics::counter!(
                            readyset_client_metrics::recorded::QUERY_LOG_TOTAL_KEYS_READ,
                            num_keys,
                            "query" => query.clone(),
                        );
                    }

                    if let Some(parse) = event.parse_duration {
                        metrics::histogram!(
                            readyset_client_metrics::recorded::QUERY_LOG_PARSE_TIME,
                            parse,
                            "query" => query.clone(),
                            "event_type" => SharedString::from(event.event),
                            "query_type" => SharedString::from(event.sql_type)
                        );
                    }

                    if let Some(readyset) = event.readyset_duration {
                        metrics::histogram!(
                            readyset_client_metrics::recorded::QUERY_LOG_EXECUTION_TIME,
                            readyset.as_secs_f64(),
                            "query" => query.clone(),
                            "database_type" => String::from(readyset_client_metrics::DatabaseType::ReadySet),
                            "event_type" => SharedString::from(event.event),
                            "query_type" => SharedString::from(event.sql_type)
                        );
                    }

                    if let Some(upstream) = event.upstream_duration {
                        metrics::histogram!(
                            readyset_client_metrics::recorded::QUERY_LOG_EXECUTION_TIME,
                            upstream.as_secs_f64(),
                            "query" => query.clone(),
                            "database_type" => String::from(readyset_client_metrics::DatabaseType::Mysql),
                            "event_type" => SharedString::from(event.event),
                            "query_type" => SharedString::from(event.sql_type)
                        );
                    }

                    if let Some(cache_misses) = event.cache_misses {
                        metrics::counter!(
                            readyset_client_metrics::recorded::QUERY_LOG_TOTAL_CACHE_MISSES,
                            cache_misses,
                            "query" => query.clone(),
                        );
                        if cache_misses != 0 {
                            metrics::counter!(
                                readyset_client_metrics::recorded::QUERY_LOG_QUERY_CACHE_MISSED,
                                1,
                                "query" => query.clone(),
                            );
                        }
                    }
                } else {
                    info!("Metrics task shutting down after request handle dropped.");
                }
            }
            _ = shutdown_recv.recv() => {
                info!("Metrics task shutting down after signal received.");
                break;
            }
        }
    }
}

impl From<DatabaseType> for readyset_client_metrics::DatabaseType {
    fn from(database_type: DatabaseType) -> Self {
        match database_type {
            DatabaseType::Mysql => readyset_client_metrics::DatabaseType::Mysql,
            DatabaseType::Psql => readyset_client_metrics::DatabaseType::Psql,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Certain clap things, like `requires`, only ever throw an error at runtime, not at
    // compile-time - this tests that none of those happen
    #[test]
    fn arg_parsing_noria_standalone() {
        let opts = Options::parse_from(vec![
            "noria-mysql",
            "--deployment",
            "test",
            "--address",
            "0.0.0.0:3306",
            "--authority-address",
            "zookeeper:2181",
            "--allow-unauthenticated-connections",
        ]);

        assert_eq!(opts.deployment, "test");
    }

    #[test]
    fn arg_parsing_with_upstream() {
        let opts = Options::parse_from(vec![
            "noria-mysql",
            "--deployment",
            "test",
            "--address",
            "0.0.0.0:3306",
            "--authority-address",
            "zookeeper:2181",
            "--allow-unauthenticated-connections",
            "--upstream-db-url",
            "mysql://root:password@mysql:3306/readyset",
        ]);

        assert_eq!(opts.deployment, "test");
    }

    #[test]
    fn async_migrations_param_defaults() {
        let opts = Options::parse_from(vec![
            "noria-mysql",
            "--deployment",
            "test",
            "--address",
            "0.0.0.0:3306",
            "--authority-address",
            "zookeeper:2181",
            "--allow-unauthenticated-connections",
            "--upstream-db-url",
            "mysql://root:password@mysql:3306/readyset",
            "--async-migrations",
        ]);

        assert_eq!(opts.max_processing_minutes, 15);
        assert_eq!(opts.migration_task_interval, 20000);
    }
}
