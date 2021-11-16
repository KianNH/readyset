use crate::*;
use mysql::prelude::Queryable;
use mysql::Value;
use noria::get_metric;
use noria::metrics::{recorded, DumpedMetricValue};
use serial_test::serial;

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn create_table_insert_test() {
    let cluster_name = "ct_create_table_insert";
    let mut deployment = DeploymentParams::new(cluster_name);
    deployment.add_server(ServerParams::default());
    deployment.add_server(ServerParams::default());
    deployment.deploy_mysql_adapter();

    let mut deployment = start_multi_process(deployment).await.unwrap();
    let opts = mysql::Opts::from_url(&deployment.mysql_connection_str().unwrap()).unwrap();
    let mut conn = mysql::Conn::new(opts.clone()).unwrap();
    let _ = conn
        .query_drop(
            r"CREATE TABLE t1 (
        uid INT NOT NULL,
        value INT NOT NULL
    );",
        )
        .unwrap();
    conn.query_drop(r"INSERT INTO t1 VALUES (1, 4);").unwrap();

    let res: Vec<(i32, i32)> = conn.query(r"SELECT * FROM t1;").unwrap();
    assert_eq!(res, vec![(1, 4)]);

    deployment.teardown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
#[ignore]
// TODO(ENG-641): Test is failing.
async fn show_tables_test() {
    let cluster_name = "ct_show_tables";
    let mut deployment = DeploymentParams::new(cluster_name);
    deployment.add_server(ServerParams::default());
    deployment.add_server(ServerParams::default());
    deployment.deploy_mysql();

    let mut deployment = start_multi_process(deployment).await.unwrap();
    let opts = mysql::Opts::from_url(&deployment.mysql_connection_str().unwrap()).unwrap();
    let mut conn = mysql::Conn::new(opts.clone()).unwrap();
    let _ = conn
        .query_drop(r"CREATE TABLE t2a (uid INT NOT NULL, value INT NOT NULL,);")
        .unwrap();
    let _ = conn
        .query_drop(r"CREATE TABLE t2b (uid INT NOT NULL, value INT NOT NULL,);")
        .unwrap();

    let tables: Vec<String> = conn.query("SHOW TABLES;").unwrap();
    deployment.teardown().await.unwrap();
    assert_eq!(tables, vec!["t2a", "t2b"]);
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
#[ignore]
// TODO(ENG-641): Test is failing.
async fn describe_table_test() {
    let cluster_name = "ct_describe_table";
    let mut deployment = DeploymentParams::new(cluster_name);
    deployment.add_server(ServerParams::default());
    deployment.add_server(ServerParams::default());
    deployment.deploy_mysql();

    let mut deployment = start_multi_process(deployment).await.unwrap();
    let opts = mysql::Opts::from_url(&deployment.mysql_connection_str().unwrap()).unwrap();
    let mut conn = mysql::Conn::new(opts.clone()).unwrap();
    let _ = conn
        .query_drop(r"CREATE TABLE t3 (uid INT NOT NULL, value INT NOT NULL,);")
        .unwrap();

    let table: Vec<mysql::Row> = conn.query("DESCRIBE t3;").unwrap();
    let descriptor = table.get(0).unwrap();
    let cols = descriptor.columns_ref();
    let cols = cols
        .iter()
        .map(|c| c.name_ref())
        .into_iter()
        .collect::<Vec<_>>();
    let vals: Vec<Value> = descriptor.clone().unwrap();

    let cols_truth = vec![
        "Field".as_bytes(),
        "Type".as_bytes(),
        "Null".as_bytes(),
        "Key".as_bytes(),
        "Default".as_bytes(),
        "Extra".as_bytes(),
    ];
    let vals_truth = vec![
        Value::Bytes("uid".as_bytes().to_vec()),
        Value::Bytes("int".as_bytes().to_vec()),
        Value::Bytes("NO".as_bytes().to_vec()),
        Value::Bytes("".as_bytes().to_vec()),
        Value::NULL,
        Value::Bytes("".as_bytes().to_vec()),
    ];

    deployment.teardown().await.unwrap();

    assert_eq!(vals, vals_truth);
    assert_eq!(cols, cols_truth);
}

/// This test verifies that a prepared statement can be executed
/// on both noria and mysql.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn mirror_prepare_exec_test() {
    let cluster_name = "ct_mirror_prepare_exec";
    let mut deployment = DeploymentParams::new(cluster_name);
    deployment.add_server(ServerParams::default());
    deployment.deploy_mysql();
    deployment.deploy_mysql_adapter();

    let mut deployment = start_multi_process(deployment).await.unwrap();

    // Create a table and write to it through the adapter.
    let opts = mysql::Opts::from_url(&deployment.mysql_connection_str().unwrap()).unwrap();
    let mut adapter_conn = mysql::Conn::new(opts.clone()).unwrap();
    adapter_conn
        .query_drop(
            r"CREATE TABLE t1 (
        uid INT NOT NULL,
        value INT NOT NULL
    );",
        )
        .unwrap();

    adapter_conn
        .query_drop(r"INSERT INTO t1 VALUES (1, 4);")
        .unwrap();
    adapter_conn
        .query_drop(r"INSERT INTO t1 VALUES (2, 5);")
        .unwrap();

    let prep_stmt = adapter_conn
        .prep(r"SELECT * FROM t1 WHERE uid = ?")
        .unwrap();
    let result: Vec<(i32, i32)> = adapter_conn.exec(prep_stmt.clone(), (2,)).unwrap();
    assert_eq!(result, vec![(2, 5)]);

    // Kill the one and only server, everything should go to fallback.
    deployment
        .kill_server(&deployment.server_addrs()[0])
        .await
        .unwrap();
    let result: Vec<(i32, i32)> = adapter_conn.exec(prep_stmt, (2,)).unwrap();
    assert_eq!(result, vec![(2, 5)]);

    deployment.teardown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn live_qca_sanity_check() {
    let cluster_name = "ct_live_qca_sanity_check";
    let mut deployment = DeploymentParams::new(cluster_name);
    deployment.add_server(ServerParams::default());
    deployment.deploy_mysql();
    deployment.deploy_mysql_adapter();
    // Enable live QCA with an interval of 500ms.
    deployment.enable_live_qca(500);

    let mut deployment = start_multi_process(deployment).await.unwrap();
    let opts = mysql::Opts::from_url(&deployment.mysql_connection_str().unwrap()).unwrap();
    let mut adapter_conn = mysql::Conn::new(opts.clone()).unwrap();
    adapter_conn
        .query_drop(
            r"CREATE TABLE t1 (
        uid INT NOT NULL,
        value INT NOT NULL
    );",
        )
        .unwrap();
    adapter_conn
        .query_drop(r"INSERT INTO t1 VALUES (1, 4);")
        .unwrap();
    adapter_conn
        .query_drop(r"INSERT INTO t1 VALUES (2, 5);")
        .unwrap();

    let prep_stmt = adapter_conn
        .prep(r"SELECT * FROM t1 WHERE uid = ?")
        .unwrap();
    let result: Vec<(i32, i32)> = adapter_conn.exec(prep_stmt.clone(), (2,)).unwrap();
    assert_eq!(result, vec![(2, 5)]);

    sleep(Duration::from_secs(4)).await;

    // Second execute should go to noria.
    let prep_stmt = adapter_conn
        .prep(r"SELECT * FROM t1 WHERE uid = ?")
        .unwrap();
    let result: Vec<(i32, i32)> = adapter_conn.exec(prep_stmt.clone(), (2,)).unwrap();
    assert_eq!(result, vec![(2, 5)]);

    // TODO(justin): Add utilities to abstract out this ridiculous way of getting
    // metrics.
    let metrics_dump = &deployment.metrics.get_metrics().await.unwrap()[0].metrics;
    assert_eq!(
        get_metric!(metrics_dump, recorded::SERVER_VIEW_QUERY_RESULT),
        Some(DumpedMetricValue::Counter(1.0))
    );

    deployment.teardown().await.unwrap();
}
