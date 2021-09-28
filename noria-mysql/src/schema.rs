use nom_sql::{self, ColumnConstraint, SqlType};

pub(crate) fn convert_column(col: &nom_sql::ColumnSpecification) -> msql_srv::Column {
    let mut colflags = msql_srv::ColumnFlags::empty();
    use msql_srv::ColumnType::*;

    let coltype = match col.sql_type {
        SqlType::Mediumtext => MYSQL_TYPE_VAR_STRING,
        SqlType::Longtext => MYSQL_TYPE_BLOB,
        SqlType::Text => MYSQL_TYPE_STRING,
        SqlType::Varchar(_) => MYSQL_TYPE_VAR_STRING,
        SqlType::Int(_) => MYSQL_TYPE_LONG,
        SqlType::UnsignedInt(_) => {
            colflags |= msql_srv::ColumnFlags::UNSIGNED_FLAG;
            MYSQL_TYPE_LONG
        }
        SqlType::Bigint(_) => MYSQL_TYPE_LONGLONG,
        SqlType::UnsignedBigint(_) => {
            colflags |= msql_srv::ColumnFlags::UNSIGNED_FLAG;
            MYSQL_TYPE_LONGLONG
        }
        SqlType::Tinyint(_) => MYSQL_TYPE_TINY,
        SqlType::UnsignedTinyint(_) => {
            colflags |= msql_srv::ColumnFlags::UNSIGNED_FLAG;
            MYSQL_TYPE_TINY
        }
        SqlType::Smallint(_) => MYSQL_TYPE_SHORT,
        SqlType::UnsignedSmallint(_) => {
            colflags |= msql_srv::ColumnFlags::UNSIGNED_FLAG;
            MYSQL_TYPE_SHORT
        }
        SqlType::Bool => MYSQL_TYPE_BIT,
        SqlType::DateTime(_) => MYSQL_TYPE_DATETIME,
        SqlType::Float => MYSQL_TYPE_FLOAT,
        SqlType::Decimal(_, _) => MYSQL_TYPE_DECIMAL,
        SqlType::Char(_) => {
            // TODO(grfn): I'm not sure if this is right
            MYSQL_TYPE_STRING
        }
        SqlType::Blob => MYSQL_TYPE_BLOB,
        SqlType::Longblob => MYSQL_TYPE_LONG_BLOB,
        SqlType::Mediumblob => MYSQL_TYPE_MEDIUM_BLOB,
        SqlType::Tinyblob => MYSQL_TYPE_TINY_BLOB,
        SqlType::Double => MYSQL_TYPE_DOUBLE,
        SqlType::Real => {
            // a generous reading of
            // https://dev.mysql.com/doc/refman/8.0/en/floating-point-types.html seems to
            // indicate that real is equivalent to float
            // TODO(grfn): Make sure that's the case
            MYSQL_TYPE_FLOAT
        }
        SqlType::Tinytext => {
            // TODO(grfn): How does the mysql binary protocol handle
            // tinytext? is it just an alias for tinyblob or is there a flag
            // we need?
            unimplemented!()
        }
        SqlType::Date => MYSQL_TYPE_DATE,
        SqlType::Timestamp => MYSQL_TYPE_TIMESTAMP,
        SqlType::Binary(_) => {
            // TODO(grfn): I don't know if this is right
            colflags |= msql_srv::ColumnFlags::BINARY_FLAG;
            MYSQL_TYPE_STRING
        }
        SqlType::Varbinary(_) => {
            // TODO(grfn): I don't know if this is right
            colflags |= msql_srv::ColumnFlags::BINARY_FLAG;
            MYSQL_TYPE_VAR_STRING
        }
        SqlType::Enum(_) => {
            // TODO(grfn): I don't know if this is right
            colflags |= msql_srv::ColumnFlags::ENUM_FLAG;
            MYSQL_TYPE_VAR_STRING
        }
        SqlType::Time => MYSQL_TYPE_TIME,
        SqlType::Json => MYSQL_TYPE_JSON,
        SqlType::ByteArray => MYSQL_TYPE_BLOB,
        SqlType::Numeric(_) => MYSQL_TYPE_DECIMAL,
        SqlType::MacAddr => unimplemented!("MySQL does not support the MACADDR type"),
    };

    for c in &col.constraints {
        match *c {
            ColumnConstraint::AutoIncrement => {
                colflags |= msql_srv::ColumnFlags::AUTO_INCREMENT_FLAG;
            }
            ColumnConstraint::NotNull => {
                colflags |= msql_srv::ColumnFlags::NOT_NULL_FLAG;
            }
            ColumnConstraint::PrimaryKey => {
                colflags |= msql_srv::ColumnFlags::PRI_KEY_FLAG;
            }
            ColumnConstraint::Unique => {
                colflags |= msql_srv::ColumnFlags::UNIQUE_KEY_FLAG;
            }
            _ => (),
        }
    }

    msql_srv::Column {
        table: col.column.table.clone().unwrap_or_default(),
        column: col.column.name.clone(),
        coltype,
        colflags,
    }
}
