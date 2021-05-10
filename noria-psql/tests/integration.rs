#[macro_use]
extern crate slog;

use chrono::{NaiveDate, NaiveDateTime};
use nom_sql::SelectStatement;
use noria_client::backend::noria_connector::NoriaConnector;
use noria_client::backend::BackendBuilder;
use noria_psql::backend::Backend;
use noria_server::{Builder, ControllerHandle, ZookeeperAuthority};
use postgres::{config::Config, Client, NoTls, SimpleQueryMessage};
use psql_srv;
use std::collections::HashMap;
use std::env;
use std::net::TcpListener;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Barrier, RwLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zookeeper::{WatchedEvent, ZooKeeper, ZooKeeperExt};

// Appends a unique ID to deployment strings, to avoid collisions between tests.
struct Deployment {
    name: String,
}

impl Deployment {
    fn new(prefix: &str) -> Self {
        let current_time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let name = format!(
            "{}.{}.{}",
            prefix,
            current_time.as_secs(),
            current_time.subsec_nanos()
        );

        Self { name }
    }
}

impl Drop for Deployment {
    fn drop(&mut self) {
        // Remove the ZK data if we created any:
        let zk = ZooKeeper::connect(
            "127.0.0.1:2181",
            Duration::from_secs(3),
            |_: WatchedEvent| {},
        );

        if let Ok(z) = zk {
            let _ = z.delete_recursive(&format!("/{}", self.name));
        }
    }
}

fn sleep() {
    thread::sleep(Duration::from_millis(200));
}

fn zk_addr() -> String {
    format!(
        "{}:{}",
        env::var("ZOOKEEPER_HOST").unwrap_or("127.0.0.1".into()),
        env::var("ZOOKEEPER_PORT").unwrap_or("2181".into()),
    )
}

// Initializes a Noria worker and starts processing PostgreSQL queries against it.
// If `partial` is `false`, disables partial queries.
fn setup(deployment: &Deployment, partial: bool) -> Config {
    // Run with VERBOSE=1 for log output.
    let verbose = env::var("VERBOSE")
        .ok()
        .and_then(|v| v.parse().ok())
        .iter()
        .any(|i| i == 1);

    let logger = if verbose {
        noria_server::logger_pls()
    } else {
        slog::Logger::root(slog::Discard, o!())
    };

    let barrier = Arc::new(Barrier::new(2));

    let l = logger.clone();
    let n = deployment.name.clone();
    let b = barrier.clone();
    thread::spawn(move || {
        let mut authority = ZookeeperAuthority::new(&format!("{}/{}", zk_addr(), n)).unwrap();
        let mut builder = Builder::default();
        if !partial {
            builder.disable_partial();
        }
        authority.log_with(l.clone());
        builder.log_with(l);
        let rt = tokio::runtime::Runtime::new().unwrap();
        // NOTE: may be important to assign to a variable here, since otherwise the handle may
        // get dropped immediately causing the Noria instance to quit.
        let _handle = rt.block_on(builder.start(Arc::new(authority))).unwrap();
        b.wait();
        loop {
            thread::sleep(Duration::from_millis(1000));
        }
    });

    barrier.wait();

    let auto_increments: Arc<RwLock<HashMap<String, AtomicUsize>>> = Arc::default();
    let query_cache: Arc<RwLock<HashMap<SelectStatement, String>>> = Arc::default();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let mut zk_auth =
        ZookeeperAuthority::new(&format!("{}/{}", zk_addr(), deployment.name)).unwrap();
    zk_auth.log_with(logger.clone());

    debug!(logger, "Connecting to Noria...",);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let ch = rt.block_on(ControllerHandle::new(zk_auth)).unwrap();
    debug!(logger, "Connected!");

    // no need for a barrier here since accept() acts as one
    thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        let s = {
            let _guard = rt.handle().enter();
            tokio::net::TcpStream::from_std(s).unwrap()
        };

        let writer = NoriaConnector::new(
            ch.clone(),
            auto_increments.clone(),
            query_cache.clone(),
            None,
        );
        let reader = NoriaConnector::new(ch, auto_increments, query_cache, None);

        let backend = Backend(
            BackendBuilder::new()
                .writer(rt.block_on(writer))
                .reader(rt.block_on(reader))
                .require_authentication(false)
                .build(),
        );

        rt.block_on(psql_srv::run_backend(backend, s));
        drop(rt);
    });

    let mut config = Client::configure();
    config
        .host("127.0.0.1")
        .port(addr.port())
        .dbname(&deployment.name);
    config
}

#[test]
fn delete_basic() {
    let d = Deployment::new("delete_basic");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE Cats (id int PRIMARY KEY, PRIMARY KEY(id))")
        .unwrap();
    sleep();

    conn.simple_query("INSERT INTO Cats (id) VALUES (1)")
        .unwrap();
    sleep();

    let row = conn
        .query_opt("SELECT Cats.id FROM Cats WHERE Cats.id = 1", &[])
        .unwrap();
    assert!(row.is_some());

    {
        let res = conn
            .simple_query("DELETE FROM Cats WHERE Cats.id = 1")
            .unwrap();
        let deleted = res.first().unwrap();
        assert!(matches!(deleted, SimpleQueryMessage::CommandComplete(1)));
        sleep();
    }

    let row = conn
        .query_opt("SELECT Cats.id FROM Cats WHERE Cats.id = 1", &[])
        .unwrap();
    assert!(row.is_none());
}

#[test]
fn delete_only_constraint() {
    let d = Deployment::new("delete_only_constraint");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    // Note that this doesn't have `id int PRIMARY KEY` like the other tests:
    conn.simple_query("CREATE TABLE Cats (id int, name VARCHAR(255), PRIMARY KEY(id))")
        .unwrap();
    sleep();

    conn.simple_query("INSERT INTO Cats (id, name) VALUES (1, \"Bob\")")
        .unwrap();
    sleep();

    {
        let res = conn
            .simple_query("DELETE FROM Cats WHERE Cats.id = 1")
            .unwrap();
        let deleted = res.first().unwrap();
        assert!(matches!(deleted, SimpleQueryMessage::CommandComplete(1)));
        sleep();
    }

    let row = conn
        .query_opt("SELECT Cats.id FROM Cats WHERE Cats.id = 1", &[])
        .unwrap();
    assert!(row.is_none());
}

#[test]
fn delete_multiple() {
    let d = Deployment::new("delete_multiple");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE Cats (id int PRIMARY KEY, PRIMARY KEY(id))")
        .unwrap();
    sleep();

    for i in 1..4 {
        conn.simple_query(&format!("INSERT INTO Cats (id) VALUES ({})", i))
            .unwrap();
        sleep();
    }

    {
        let res = conn
            .simple_query("DELETE FROM Cats WHERE Cats.id = 1 OR Cats.id = 2")
            .unwrap();
        let deleted = res.first().unwrap();
        assert!(matches!(deleted, SimpleQueryMessage::CommandComplete(2)));
        sleep();
    }

    let row = conn
        .query_opt("SELECT Cats.id FROM Cats WHERE Cats.id = 1", &[])
        .unwrap();
    assert!(row.is_none());

    let row = conn
        .query_opt("SELECT Cats.id FROM Cats WHERE Cats.id = 2", &[])
        .unwrap();
    assert!(row.is_none());

    let row = conn
        .query_opt("SELECT Cats.id FROM Cats WHERE Cats.id = 3", &[])
        .unwrap();
    assert!(row.is_some());
}

#[test]
fn delete_bogus() {
    let d = Deployment::new("delete_bogus");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE Cats (id int PRIMARY KEY, PRIMARY KEY(id))")
        .unwrap();
    sleep();

    // `id` can't be both 1 and 2!
    {
        let res = conn
            .simple_query("DELETE FROM Cats WHERE Cats.id = 1 AND Cats.id = 2")
            .unwrap();
        let deleted = res.first().unwrap();
        assert!(matches!(deleted, SimpleQueryMessage::CommandComplete(0)));
        sleep();
    }
}

#[test]
fn delete_bogus_valid_and() {
    let d = Deployment::new("delete_bogus_valid_and");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE Cats (id int PRIMARY KEY, PRIMARY KEY(id))")
        .unwrap();
    sleep();

    conn.simple_query("INSERT INTO Cats (id) VALUES (1)")
        .unwrap();
    sleep();

    let row = conn
        .query_opt("SELECT Cats.id FROM Cats WHERE Cats.id = 1", &[])
        .unwrap();
    assert!(row.is_some());

    // Not that it makes much sense, but we should support this regardless...
    {
        let res = conn
            .simple_query("DELETE FROM Cats WHERE Cats.id = 1 AND Cats.id = 1")
            .unwrap();
        let deleted = res.first().unwrap();
        assert!(matches!(deleted, SimpleQueryMessage::CommandComplete(1)));
        sleep();
    }

    let row = conn
        .query_opt("SELECT Cats.id FROM Cats WHERE Cats.id = 1", &[])
        .unwrap();
    assert!(row.is_none());
}

#[test]
fn delete_bogus_valid_or() {
    let d = Deployment::new("delete_bogus_valid_or");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE Cats (id int PRIMARY KEY, PRIMARY KEY(id))")
        .unwrap();
    sleep();

    conn.simple_query("INSERT INTO Cats (id) VALUES (1)")
        .unwrap();
    sleep();

    let row = conn
        .query_opt("SELECT Cats.id FROM Cats WHERE Cats.id = 1", &[])
        .unwrap();
    assert!(row.is_some());

    // Not that it makes much sense, but we should support this regardless...
    {
        let res = conn
            .simple_query("DELETE FROM Cats WHERE Cats.id = 1 OR Cats.id = 1")
            .unwrap();
        let deleted = res.first().unwrap();
        assert!(matches!(deleted, SimpleQueryMessage::CommandComplete(1)));
        sleep();
    }

    let row = conn
        .query_opt("SELECT Cats.id FROM Cats WHERE Cats.id = 1", &[])
        .unwrap();
    assert!(row.is_none());
}

#[test]
fn delete_other_column() {
    let d = Deployment::new("delete_other_column");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE Cats (id int PRIMARY KEY, name VARCHAR(255), PRIMARY KEY(id))")
        .unwrap();
    sleep();

    assert!(matches!(
        conn.simple_query("DELETE FROM Cats WHERE Cats.id = 1 OR Cats.name = \"Bob\""),
        Err(_)
    ));
}

#[test]
fn delete_no_keys() {
    let d = Deployment::new("delete_no_keys");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE Cats (id int PRIMARY KEY, name VARCHAR(255), PRIMARY KEY(id))")
        .unwrap();
    sleep();

    assert!(matches!(
        conn.simple_query("DELETE FROM Cats WHERE 1 = 1"),
        Err(_)
    ));
}

#[test]
fn delete_compound_primary_key() {
    let d = Deployment::new("delete_compound_primary_key");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query(
        "CREATE TABLE Vote (aid int, uid int, reason VARCHAR(255), PRIMARY KEY(aid, uid))",
    )
    .unwrap();
    sleep();

    conn.simple_query("INSERT INTO Vote (aid, uid) VALUES (1, 2)")
        .unwrap();
    conn.simple_query("INSERT INTO Vote (aid, uid) VALUES (1, 3)")
        .unwrap();
    sleep();

    {
        let res = conn
            .simple_query("DELETE FROM Vote WHERE Vote.aid = 1 AND Vote.uid = 2")
            .unwrap();
        let deleted = res.first().unwrap();
        assert!(matches!(deleted, SimpleQueryMessage::CommandComplete(1)));
        sleep();
    }

    let row = conn
        .query_opt(
            "SELECT Vote.uid FROM Vote WHERE Vote.aid = 1 AND Vote.uid = 2",
            &[],
        )
        .unwrap();
    assert!(row.is_none());

    let uid: i32 = conn
        .query_one(
            "SELECT Vote.uid FROM Vote WHERE Vote.aid = 1 AND Vote.uid = 3",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(3, uid);
}

#[test]
fn update_basic() {
    let d = Deployment::new("update_basic");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE Cats (id int PRIMARY KEY, name VARCHAR(255), PRIMARY KEY(id))")
        .unwrap();
    sleep();

    conn.simple_query("INSERT INTO Cats (id, name) VALUES (1, \"Bob\")")
        .unwrap();
    sleep();

    {
        let updated = conn
            .execute(
                "UPDATE Cats SET Cats.name = \"Rusty\" WHERE Cats.id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(updated, 1);
        sleep();
    }

    let name: String = conn
        .query_one("SELECT Cats.name FROM Cats WHERE Cats.id = 1", &[])
        .unwrap()
        .get(0);
    assert_eq!(name, String::from("Rusty"));
}

#[test]
fn update_basic_prepared() {
    let d = Deployment::new("update_basic");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE Cats (id int PRIMARY KEY, name VARCHAR(255), PRIMARY KEY(id))")
        .unwrap();
    sleep();

    conn.simple_query("INSERT INTO Cats (id, name) VALUES (1, \"Bob\")")
        .unwrap();
    sleep();

    {
        let updated = conn
            .execute(
                "UPDATE Cats SET Cats.name = \"Rusty\" WHERE Cats.id = $1",
                &[&1],
            )
            .unwrap();
        assert_eq!(updated, 1);
        sleep();
    }

    let name: String = conn
        .query_one("SELECT Cats.name FROM Cats WHERE Cats.id = 1", &[])
        .unwrap()
        .get(0);
    assert_eq!(name, String::from("Rusty"));

    {
        let updated = conn
            .execute(
                "UPDATE Cats SET Cats.name = $1 WHERE Cats.id = $2",
                &[&"Bob", &1],
            )
            .unwrap();
        assert_eq!(updated, 1);
        sleep();
    }

    let name: String = conn
        .query_one("SELECT Cats.name FROM Cats WHERE Cats.id = 1", &[])
        .unwrap()
        .get(0);
    assert_eq!(name, String::from("Bob"));
}

#[test]
fn update_compound_primary_key() {
    let d = Deployment::new("update_compound_primary_key");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query(
        "CREATE TABLE Vote (aid int, uid int, reason VARCHAR(255), PRIMARY KEY(aid, uid))",
    )
    .unwrap();
    sleep();

    conn.simple_query("INSERT INTO Vote (aid, uid, reason) VALUES (1, 2, \"okay\")")
        .unwrap();
    conn.simple_query("INSERT INTO Vote (aid, uid, reason) VALUES (1, 3, \"still okay\")")
        .unwrap();
    sleep();

    {
        let updated = conn
            .execute(
                "UPDATE Vote SET Vote.reason = \"better\" WHERE Vote.aid = 1 AND Vote.uid = 2",
                &[],
            )
            .unwrap();
        assert_eq!(updated, 1);
        sleep();
    }

    let name: String = conn
        .query_one(
            "SELECT Vote.reason FROM Vote WHERE Vote.aid = 1 AND Vote.uid = 2",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(name, String::from("better"));

    let name: String = conn
        .query_one(
            "SELECT Vote.reason FROM Vote WHERE Vote.aid = 1 AND Vote.uid = 3",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(name, String::from("still okay"));
}

#[test]
fn update_only_constraint() {
    let d = Deployment::new("update_only_constraint");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    // Note that this doesn't have `id int PRIMARY KEY` like the other tests:
    conn.simple_query("CREATE TABLE Cats (id int, name VARCHAR(255), PRIMARY KEY(id))")
        .unwrap();
    sleep();

    conn.simple_query("INSERT INTO Cats (id, name) VALUES (1, \"Bob\")")
        .unwrap();
    sleep();

    {
        let updated = conn
            .execute(
                "UPDATE Cats SET Cats.name = \"Rusty\" WHERE Cats.id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(updated, 1);
        sleep();
    }

    let name: String = conn
        .query_one("SELECT Cats.name FROM Cats WHERE Cats.id = 1", &[])
        .unwrap()
        .get(0);
    assert_eq!(name, String::from("Rusty"));
}

#[test]
fn update_pkey() {
    let d = Deployment::new("update_pkey");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE Cats (id int PRIMARY KEY, name VARCHAR(255), PRIMARY KEY(id))")
        .unwrap();
    sleep();

    conn.simple_query("INSERT INTO Cats (id, name) VALUES (1, \"Bob\")")
        .unwrap();
    sleep();

    {
        let updated = conn
            .execute(
                "UPDATE Cats SET Cats.name = \"Rusty\", Cats.id = 10 WHERE Cats.id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(updated, 1);
        sleep();
    }

    let name: String = conn
        .query_one("SELECT Cats.name FROM Cats WHERE Cats.id = 10", &[])
        .unwrap()
        .get(0);
    assert_eq!(name, String::from("Rusty"));

    let old_row = conn
        .query_opt("SELECT Cats.name FROM Cats WHERE Cats.id = 1", &[])
        .unwrap();
    assert!(old_row.is_none());
}

#[test]
fn update_separate() {
    let d = Deployment::new("update_separate");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE Cats (id int PRIMARY KEY, name VARCHAR(255), PRIMARY KEY(id))")
        .unwrap();
    sleep();

    conn.simple_query("INSERT INTO Cats (id, name) VALUES (1, \"Bob\")")
        .unwrap();
    sleep();

    {
        let updated = conn
            .execute(
                "UPDATE Cats SET Cats.name = \"Rusty\" WHERE Cats.id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(updated, 1);
        sleep();
    }

    {
        let updated = conn
            .execute(
                "UPDATE Cats SET Cats.name = \"Rusty II\" WHERE Cats.id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(updated, 1);
        sleep();
    }

    let name: String = conn
        .query_one("SELECT Cats.name FROM Cats WHERE Cats.id = 1", &[])
        .unwrap()
        .get(0);
    assert_eq!(name, String::from("Rusty II"));
}

#[test]
fn update_no_keys() {
    let d = Deployment::new("update_no_keys");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE Cats (id int PRIMARY KEY, name VARCHAR(255), PRIMARY KEY(id))")
        .unwrap();
    sleep();

    let query = "UPDATE Cats SET Cats.name = \"Rusty\" WHERE 1 = 1";
    assert!(matches!(conn.simple_query(query), Err(_)));
}

#[test]
fn update_other_column() {
    let d = Deployment::new("update_no_keys");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE Cats (id int PRIMARY KEY, name VARCHAR(255), PRIMARY KEY(id))")
        .unwrap();
    sleep();

    let query = "UPDATE Cats SET Cats.name = \"Rusty\" WHERE Cats.name = \"Bob\"";
    assert!(matches!(conn.simple_query(query), Err(_)));
}

#[test]
fn update_bogus() {
    let d = Deployment::new("update_bogus");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE Cats (id int PRIMARY KEY, name VARCHAR(255), PRIMARY KEY(id))")
        .unwrap();
    sleep();

    conn.simple_query("INSERT INTO Cats (id, name) VALUES (1, \"Bob\")")
        .unwrap();
    sleep();

    let query = "UPDATE Cats SET Cats.name = \"Rusty\" WHERE Cats.id = 1 AND Cats.id = 2";
    assert!(matches!(conn.simple_query(query), Err(_)));
}

#[test]
fn select_collapse_where_in() {
    let d = Deployment::new("collapsed_where");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE Cats (id int PRIMARY KEY, name VARCHAR(255), PRIMARY KEY(id))")
        .unwrap();
    sleep();

    conn.simple_query("INSERT INTO Cats (id, name) VALUES (1, \"Bob\")")
        .unwrap();
    conn.simple_query("INSERT INTO Cats (id, name) VALUES (2, \"Jane\")")
        .unwrap();
    sleep();

    // NOTE: It seems that Noria may require WHERE IN prepared statements to contain at least one
    // parameter. For that reason, simple_query is used instead.
    let names: Vec<String> = conn
        .simple_query("SELECT Cats.name FROM Cats WHERE Cats.id IN (1, 2)")
        .unwrap()
        .into_iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some(r.get(0).unwrap().to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(names.len(), 2);
    assert!(names.iter().any(|s| s == "Bob"));
    assert!(names.iter().any(|s| s == "Jane"));

    let names: Vec<String> = conn
        .query(
            "SELECT Cats.name FROM Cats WHERE Cats.id IN ($1, $2)",
            &[&1, &2],
        )
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(names.len(), 2);
    assert!(names.iter().any(|s| s == "Bob"));
    assert!(names.iter().any(|s| s == "Jane"));

    // some lookups give empty results
    let names: Vec<String> = conn
        .simple_query("SELECT Cats.name FROM Cats WHERE Cats.id IN (1, 2, 3)")
        .unwrap()
        .into_iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some(r.get(0).unwrap().to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(names.len(), 2);
    assert!(names.iter().any(|s| s == "Bob"));
    assert!(names.iter().any(|s| s == "Jane"));

    let names: Vec<String> = conn
        .query(
            "SELECT Cats.name FROM Cats WHERE Cats.id IN ($1, $2, $3)",
            &[&1, &2, &3],
        )
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(names.len(), 2);
    assert!(names.iter().any(|s| s == "Bob"));
    assert!(names.iter().any(|s| s == "Jane"));

    // also track another parameter
    let names: Vec<String> = conn
        .simple_query("SELECT Cats.name FROM Cats WHERE Cats.name = 'Bob' AND Cats.id IN (1, 2)")
        .unwrap()
        .into_iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some(r.get(0).unwrap().to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(names.len(), 1);
    assert!(names.iter().any(|s| s == "Bob"));

    let names: Vec<String> = conn
        .query(
            "SELECT Cats.name FROM Cats WHERE Cats.name = $1 AND Cats.id IN ($2, $3)",
            &[&"Bob".to_string(), &1, &2],
        )
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(names.len(), 1);
    assert!(names.iter().any(|s| s == "Bob"));
}

#[test]
fn basic_select() {
    let d = Deployment::new("basic_select");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE test (x int, y int)")
        .unwrap();
    sleep();

    conn.simple_query("INSERT INTO test (x, y) VALUES (4, 2)")
        .unwrap();
    sleep();

    // Test binary format response.
    let rows = conn.query("SELECT test.* FROM test", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    let row = rows.first().unwrap();
    assert_eq!(row.len(), 2);
    assert_eq!(row.get::<usize, i32>(0), 4);
    assert_eq!(row.get::<usize, i32>(1), 2);

    // Test text format response.
    let rows = conn.simple_query("SELECT test.* FROM test").unwrap();
    let row = match rows.first().unwrap() {
        SimpleQueryMessage::Row(r) => r,
        _ => panic!(),
    };
    assert_eq!(row.get(0).unwrap(), "4");
    assert_eq!(row.get(1).unwrap(), "2");
}

#[test]
fn strings() {
    let d = Deployment::new("strings");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE test (x TEXT)").unwrap();
    sleep();

    conn.simple_query("INSERT INTO test (x) VALUES ('foo')")
        .unwrap();
    sleep();

    // Test binary format response.
    let rows = conn.query("SELECT test.* FROM test", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    let row = rows.first().unwrap();
    assert_eq!(row.len(), 1);
    assert_eq!(row.get::<usize, String>(0), "foo".to_string());

    // Test text format response.
    let rows = conn.simple_query("SELECT test.* FROM test").unwrap();
    let row = match rows.first().unwrap() {
        SimpleQueryMessage::Row(r) => r,
        _ => panic!(),
    };
    assert_eq!(row.get(0).unwrap(), "foo");
}

#[test]
fn prepared_select() {
    let d = Deployment::new("prepared_select");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE test (x int, y int)")
        .unwrap();
    sleep();

    conn.simple_query("INSERT INTO test (x, y) VALUES (4, 2)")
        .unwrap();
    sleep();

    let rows = conn
        .query("SELECT test.* FROM test WHERE x = $1", &[&4])
        .unwrap();
    assert_eq!(rows.len(), 1);
    let row = rows.first().unwrap();
    assert_eq!(row.len(), 2);
    assert_eq!(row.get::<usize, i32>(0), 4);
    assert_eq!(row.get::<usize, i32>(1), 2);
}

#[test]
fn create_view() {
    let d = Deployment::new("create_view");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE test (x int, y int)")
        .unwrap();
    sleep();

    conn.simple_query("INSERT INTO test (x, y) VALUES (4, 2)")
        .unwrap();
    sleep();

    conn.simple_query("CREATE VIEW testview AS SELECT test.* FROM test")
        .unwrap();
    sleep();

    let rows = conn.query("SELECT testview.* FROM testview", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    let row = rows.first().unwrap();
    assert_eq!(row.len(), 2);
    assert_eq!(row.get::<usize, i32>(0), 4);
    assert_eq!(row.get::<usize, i32>(1), 2);

    let rows = conn.query("SELECT test.* FROM test", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    let row = rows.first().unwrap();
    assert_eq!(row.len(), 2);
    assert_eq!(row.get::<usize, i32>(0), 4);
    assert_eq!(row.get::<usize, i32>(1), 2);
}

#[test]
fn absurdly_simple_select() {
    let d = Deployment::new("absurdly_simple_select");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE test (x int, y int)")
        .unwrap();
    sleep();

    conn.simple_query("INSERT INTO test (x, y) VALUES (4, 2)")
        .unwrap();
    conn.simple_query("INSERT INTO test (x, y) VALUES (1, 3)")
        .unwrap();
    conn.simple_query("INSERT INTO test (x, y) VALUES (2, 4)")
        .unwrap();
    sleep();

    let rows = conn.query("SELECT * FROM test", &[]).unwrap();
    let mut rows: Vec<(i32, i32)> = rows
        .iter()
        .map(|r| (r.get::<usize, i32>(0), r.get::<usize, i32>(1)))
        .collect();
    rows.sort();
    assert_eq!(rows, vec![(1, 3), (2, 4), (4, 2)]);
}

#[test]
fn order_by_basic() {
    let d = Deployment::new("order_by_basic");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE test (x int, y int)")
        .unwrap();
    sleep();

    conn.simple_query("INSERT INTO test (x, y) VALUES (4, 2)")
        .unwrap();
    conn.simple_query("INSERT INTO test (x, y) VALUES (1, 3)")
        .unwrap();
    conn.simple_query("INSERT INTO test (x, y) VALUES (2, 4)")
        .unwrap();
    sleep();

    let mut rows: Vec<(i32, i32)> = conn
        .query("SELECT * FROM test", &[])
        .unwrap()
        .iter()
        .map(|r| (r.get::<usize, i32>(0), r.get::<usize, i32>(1)))
        .collect();
    rows.sort();
    assert_eq!(rows, vec![(1, 3), (2, 4), (4, 2)]);
    let rows: Vec<(i32, i32)> = conn
        .query("SELECT * FROM test ORDER BY x DESC", &[])
        .unwrap()
        .iter()
        .map(|r| (r.get::<usize, i32>(0), r.get::<usize, i32>(1)))
        .collect();
    assert_eq!(rows, vec![(4, 2), (2, 4), (1, 3)]);
    let rows: Vec<(i32, i32)> = conn
        .query("SELECT * FROM test ORDER BY y ASC", &[])
        .unwrap()
        .iter()
        .map(|r| (r.get::<usize, i32>(0), r.get::<usize, i32>(1)))
        .collect();
    assert_eq!(rows, vec![(4, 2), (1, 3), (2, 4)]);
    let rows: Vec<(i32, i32)> = conn
        .query("SELECT * FROM test ORDER BY y DESC", &[])
        .unwrap()
        .iter()
        .map(|r| (r.get::<usize, i32>(0), r.get::<usize, i32>(1)))
        .collect();
    assert_eq!(rows, vec![(2, 4), (1, 3), (4, 2)]);
}

#[test]
fn order_by_limit_basic() {
    let d = Deployment::new("order_by_limit_basic");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE test (x int, y int)")
        .unwrap();
    sleep();

    conn.simple_query("INSERT INTO test (x, y) VALUES (4, 2)")
        .unwrap();
    conn.simple_query("INSERT INTO test (x, y) VALUES (1, 3)")
        .unwrap();
    conn.simple_query("INSERT INTO test (x, y) VALUES (2, 4)")
        .unwrap();
    sleep();

    let mut rows: Vec<(i32, i32)> = conn
        .query("SELECT * FROM test", &[])
        .unwrap()
        .iter()
        .map(|r| (r.get::<usize, i32>(0), r.get::<usize, i32>(1)))
        .collect();
    rows.sort();
    assert_eq!(rows, vec![(1, 3), (2, 4), (4, 2)]);
    let rows: Vec<(i32, i32)> = conn
        .query("SELECT * FROM test ORDER BY x DESC LIMIT 3", &[])
        .unwrap()
        .iter()
        .map(|r| (r.get::<usize, i32>(0), r.get::<usize, i32>(1)))
        .collect();
    assert_eq!(rows, vec![(4, 2), (2, 4), (1, 3)]);
}

#[test]
fn write_timestamps() {
    let d = Deployment::new("insert_timestamps");
    let opts = setup(&d, true);
    let mut conn = opts.connect(NoTls).unwrap();
    conn.simple_query("CREATE TABLE posts (id int primary key, created_at TIMESTAMP)")
        .unwrap();
    conn.simple_query("INSERT INTO posts (id, created_at) VALUES (1, '2020-01-23 17:08:24')")
        .unwrap();

    // Test binary format response.
    let row = conn
        .query_one("SELECT id, created_at FROM posts WHERE id = $1", &[&1])
        .unwrap();
    assert_eq!(row.get::<usize, i32>(0), 1);
    assert_eq!(
        row.get::<usize, NaiveDateTime>(1),
        NaiveDate::from_ymd(2020, 1, 23).and_hms(17, 08, 24)
    );

    // Test text format response.
    let rows = conn
        .simple_query("SELECT id, created_at FROM posts")
        .unwrap();
    let row = match rows.first().unwrap() {
        SimpleQueryMessage::Row(r) => r,
        _ => panic!(),
    };
    assert_eq!(row.get(0).unwrap(), "1");
    assert_eq!(row.get(1).unwrap(), "2020-01-23 17:08:24");

    {
        let updated = conn
            .execute(
                "UPDATE posts SET created_at = '2021-01-25 17:08:24' WHERE id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(updated, 1);
        sleep();
    }

    let row = conn
        .query_one("SELECT id, created_at FROM posts WHERE id = $1", &[&1])
        .unwrap();
    assert_eq!(row.get::<usize, i32>(0), 1);
    assert_eq!(
        row.get::<usize, NaiveDateTime>(1),
        NaiveDate::from_ymd(2021, 1, 25).and_hms(17, 08, 24)
    );
}
