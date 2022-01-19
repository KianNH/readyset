use crate::controller::sql::query_utils::LogicalOp;
use common::IndexType;
use nom_sql::analysis::ReferredColumns;
use nom_sql::{
    BinaryOperator, Column, Expression, FieldDefinitionExpression, InValue, ItemPlaceholder,
    JoinConstraint, JoinOperator, JoinRightSide, Literal, Table, UnaryOperator,
};
use nom_sql::{OrderType, SelectStatement};
use noria::PlaceholderIdx;
use noria_errors::{internal, invariant, invariant_eq, unsupported, ReadySetResult};

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::hash::{Hash, Hasher};
use std::mem;
use std::string::String;
use std::vec::Vec;

use super::mir;
use super::query_utils::{is_aggregate, is_predicate, map_aggregates};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct LiteralColumn {
    pub name: String,
    pub table: Option<String>,
    pub value: Literal,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct ExpressionColumn {
    pub name: String,
    pub table: Option<String>,
    pub expression: Expression,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum OutputColumn {
    Data { alias: String, column: Column },
    Literal(LiteralColumn),
    Expression(ExpressionColumn),
}

impl Ord for OutputColumn {
    fn cmp(&self, other: &OutputColumn) -> Ordering {
        match *self {
            OutputColumn::Expression(ExpressionColumn {
                ref name,
                ref table,
                ..
            })
            | OutputColumn::Data {
                column:
                    Column {
                        ref name,
                        ref table,
                        ..
                    },
                ..
            }
            | OutputColumn::Literal(LiteralColumn {
                ref name,
                ref table,
                ..
            }) => match *other {
                OutputColumn::Expression(ExpressionColumn {
                    name: ref other_name,
                    table: ref other_table,
                    ..
                })
                | OutputColumn::Data {
                    column:
                        Column {
                            name: ref other_name,
                            table: ref other_table,
                            ..
                        },
                    ..
                }
                | OutputColumn::Literal(LiteralColumn {
                    name: ref other_name,
                    table: ref other_table,
                    ..
                }) => {
                    if table.is_some() && other_table.is_some() {
                        match table.cmp(other_table) {
                            Ordering::Equal => name.cmp(other_name),
                            x => x,
                        }
                    } else {
                        name.cmp(other_name)
                    }
                }
            },
        }
    }
}

impl PartialOrd for OutputColumn {
    fn partial_cmp(&self, other: &OutputColumn) -> Option<Ordering> {
        match *self {
            OutputColumn::Expression(ExpressionColumn {
                ref name,
                ref table,
                ..
            })
            | OutputColumn::Data {
                column:
                    Column {
                        ref name,
                        ref table,
                        ..
                    },
                ..
            }
            | OutputColumn::Literal(LiteralColumn {
                ref name,
                ref table,
                ..
            }) => match *other {
                OutputColumn::Expression(ExpressionColumn {
                    name: ref other_name,
                    table: ref other_table,
                    ..
                })
                | OutputColumn::Data {
                    column:
                        Column {
                            name: ref other_name,
                            table: ref other_table,
                            ..
                        },
                    ..
                }
                | OutputColumn::Literal(LiteralColumn {
                    name: ref other_name,
                    table: ref other_table,
                    ..
                }) => {
                    if table.is_some() && other_table.is_some() {
                        match table.cmp(other_table) {
                            Ordering::Equal => Some(name.cmp(other_name)),
                            x => Some(x),
                        }
                    } else if table.is_none() && other_table.is_none() {
                        Some(name.cmp(other_name))
                    } else {
                        None
                    }
                }
            },
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Serialize, Deserialize)]
pub struct JoinRef {
    pub src: String,
    pub dst: String,
}

/// An equality predicate on two expressions, used as the key for a join
#[derive(Clone, Debug, Hash, PartialEq, Serialize, Deserialize)]
pub struct JoinPredicate {
    pub left: Expression,
    pub right: Expression,
}

/// An individual column on which a query is parameterized
#[derive(Clone, Debug, Hash, PartialEq, Serialize, Deserialize)]
pub struct Parameter {
    pub col: Column,
    pub op: nom_sql::BinaryOperator,
    pub placeholder_idx: Option<PlaceholderIdx>,
}

#[derive(Clone, Debug, Hash, PartialEq, Serialize, Deserialize)]
pub struct QueryGraphNode {
    pub rel_name: String,
    pub predicates: Vec<Expression>,
    pub columns: Vec<Column>,
    pub parameters: Vec<Parameter>,
}

#[derive(Clone, Debug, Hash, PartialEq, Serialize, Deserialize)]
pub enum QueryGraphEdge {
    Join { on: Vec<JoinPredicate> },
    LeftJoin { on: Vec<JoinPredicate> },
    GroupBy(Vec<Column>),
}

#[derive(Clone, Debug, Hash, PartialEq, Serialize, Deserialize)]
pub struct Pagination {
    pub order: Vec<(Column, OrderType)>,
    pub limit: Option<Expression>,
    pub offset: Option<Expression>,
}

/// Description of the lookup key for a view
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewKey {
    /// The list of key columns for the view, and for each column a description of how that column
    /// maps back to a placeholder in the original query, if at all
    pub columns: Vec<(mir::Column, Option<PlaceholderIdx>)>,

    /// The selected index type for the view
    pub index_type: IndexType,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct QueryGraph {
    /// Relations mentioned in the query.
    pub relations: HashMap<String, QueryGraphNode>,
    /// Joins and GroupBys in the query.
    #[serde(with = "serde_with::rust::hashmap_as_tuple_list")]
    pub edges: HashMap<(String, String), QueryGraphEdge>,
    /// Final set of projected columns in this query; may include literals in addition to the
    /// columns reflected in individual relations' `QueryGraphNode` structures.
    pub columns: Vec<OutputColumn>,
    /// Establishes an order for join predicates. Each join predicate can be identified by
    /// its (src, dst) pair
    pub join_order: Vec<JoinRef>,
    /// Global predicates (not associated with a particular relation)
    pub global_predicates: Vec<Expression>,
    /// The pagination (order, limit, offset) for the query, if any
    pub pagination: Option<Pagination>,
}

impl QueryGraph {
    fn new() -> Self {
        Default::default()
    }

    /// Returns the set of columns on which this query is parameterized. They can come from
    /// multiple tables involved in the query.
    pub fn parameters(&self) -> Vec<&Parameter> {
        self.relations
            .values()
            .flat_map(|qgn| qgn.parameters.iter())
            .collect()
    }

    pub fn exact_hash(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;

        let mut s = DefaultHasher::new();
        self.hash(&mut s);
        s.finish()
    }

    /// Returns true if the query that this query graph represents has any aggregates
    pub fn has_aggregates(&self) -> bool {
        self.relations
            .get("computed_columns")
            .iter()
            .any(|rel| !rel.columns.is_empty())
    }

    /// Construct a representation of the lookup key of a view for this query graph, based on the
    /// parameters in this query.
    pub(crate) fn view_key(&self) -> ReadySetResult<ViewKey> {
        if self.parameters().is_empty() {
            Ok(ViewKey {
                columns: vec![(mir::Column::new(None, "bogokey"), None)],
                index_type: IndexType::HashMap,
            })
        } else {
            let has_aggregates = self.has_aggregates();
            let mut index_type = None;
            for param in self.parameters() {
                // Aggregates don't currently work with range queries (since we don't
                // re-aggregate at the reader), so check here and return an error if the
                // query has both aggregates and range params
                if has_aggregates && param.op != BinaryOperator::Equal {
                    unsupported!("Aggregates are not currently supported with non-equal parameters")
                }

                match IndexType::for_operator(param.op) {
                    Some(it) if index_type.is_none() => index_type = Some(it),
                    Some(it) if index_type == Some(it) => {}
                    Some(_) => unsupported!("Conflicting binary operators in query"),
                    None => unsupported!("Unsupported binary operator `{}`", param.op),
                }
            }

            Ok(ViewKey {
                columns: self
                    .parameters()
                    .into_iter()
                    .map(|param| (mir::Column::from(param.col.clone()), param.placeholder_idx))
                    .collect(),
                index_type: index_type.expect("Checked self.parameters() isn't empty above"),
            })
        }
    }
}

#[allow(clippy::derive_hash_xor_eq)]
impl Hash for QueryGraph {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // sorted iteration over relations, edges to ensure consistent hash
        let mut rels: Vec<(&String, &QueryGraphNode)> = self.relations.iter().collect();
        rels.sort_by(|a, b| a.0.cmp(b.0));
        rels.hash(state);
        let mut edges: Vec<(&(String, String), &QueryGraphEdge)> = self.edges.iter().collect();
        edges.sort_by(|a, b| match (a.0).0.cmp(&(b.0).0) {
            Ordering::Equal => (a.0).1.cmp(&(b.0).1),
            x => x,
        });
        edges.hash(state);

        // columns and join_order are Vecs, so already ordered
        self.columns.hash(state);
        self.join_order.hash(state);
        self.global_predicates.hash(state);
        self.pagination.hash(state);
    }
}

/// Splits top level conjunctions into multiple predicates
fn split_conjunctions(ces: Vec<Expression>) -> Vec<Expression> {
    let mut new_ces = Vec::new();
    for ce in ces {
        match ce {
            Expression::BinaryOp {
                op: BinaryOperator::And,
                lhs,
                rhs,
            } => {
                new_ces.extend(split_conjunctions(vec![*lhs.clone()]));
                new_ces.extend(split_conjunctions(vec![*rhs.clone()]));
            }
            _ => {
                new_ces.push(ce.clone());
            }
        }
    }

    new_ces
}

// 1. Extract any predicates with placeholder parameters. We push these down to the edge
//    nodes, since we cannot instantiate the parameters inside the data flow graph (except for
//    non-materialized nodes).
// 2. Extract local predicates
// 3. Extract join predicates
// 4. Collect remaining predicates as global predicates
fn classify_conditionals(
    ce: &Expression,
    tables: &[Table],
    local: &mut HashMap<String, Vec<Expression>>,
    join: &mut Vec<JoinPredicate>,
    global: &mut Vec<Expression>,
    params: &mut Vec<Parameter>,
) -> ReadySetResult<()> {
    // Handling OR and AND expressions requires some care as there are some corner cases.
    //    a) we don't support OR expressions with predicates with placeholder parameters,
    //       because these expressions are meaningless in the Soup context.
    //    b) we don't support OR expressions with join predicates because they are weird and
    //       too hard.
    //    c) we don't support OR expressions between different tables (e.g table1.x = 1 OR
    //       table2.y= 42). this is a global predicate according to finkelstein algorithm
    //       and we don't support these yet.

    match ce {
        Expression::BinaryOp { op, lhs, rhs } => {
            if let Ok(op) = LogicalOp::try_from(*op) {
                // first, we recurse on both sides, collected the result of nested predicate analysis
                // in separate collections. What do do with these depends on whether we're an AND or an
                // OR clause:
                //  1) AND can be split into separate local predicates one one or more tables
                //  2) OR predictes must be preserved in their entirety, and we only use the nested
                //     local predicates discovered to decide if the OR is over one table (so it can
                //     remain a local predicate) or over several (so it must be a global predicate)
                let mut new_params = Vec::new();
                let mut new_join = Vec::new();
                let mut new_local = HashMap::new();
                let mut new_global = Vec::new();

                classify_conditionals(
                    lhs.as_ref(),
                    tables,
                    &mut new_local,
                    &mut new_join,
                    &mut new_global,
                    &mut new_params,
                )?;
                classify_conditionals(
                    rhs.as_ref(),
                    tables,
                    &mut new_local,
                    &mut new_join,
                    &mut new_global,
                    &mut new_params,
                )?;

                match op {
                    LogicalOp::And => {
                        //
                        for (t, ces) in new_local {
                            // conjunction, check if either side had a local predicate
                            invariant!(
                                ces.len() <= 2,
                                "can only combine two or fewer ConditionExpression's"
                            );
                            if ces.len() == 2 {
                                let new_ce = Expression::BinaryOp {
                                    op: BinaryOperator::And,
                                    lhs: Box::new(ces.first().unwrap().clone()),
                                    rhs: Box::new(ces.last().unwrap().clone()),
                                };

                                let e = local.entry(t.to_string()).or_default();
                                e.push(new_ce);
                            } else {
                                let e = local.entry(t.to_string()).or_default();
                                e.extend(ces);
                            }
                        }

                        // one side of the AND might be a global predicate, so we need to keep
                        // new_global around
                        global.extend(new_global);
                    }
                    LogicalOp::Or => {
                        if !new_join.is_empty() {
                            unsupported!("can't handle OR expressions between JOIN predicates")
                        }
                        if !new_params.is_empty() {
                            unsupported!(
                                "can't handle OR expressions between query parameter predicates"
                            );
                        }
                        if new_local.keys().len() == 1 && new_global.is_empty() {
                            // OR over a single table => local predicate
                            let (t, ces) = new_local.into_iter().next().unwrap();
                            if ces.len() != 2 {
                                unsupported!("should combine only 2 ConditionExpressions");
                            }
                            let new_ce = Expression::BinaryOp {
                                lhs: Box::new(ces.first().unwrap().clone()),
                                op: BinaryOperator::Or,
                                rhs: Box::new(ces.last().unwrap().clone()),
                            };

                            let e = local.entry(t).or_default();
                            e.push(new_ce);
                        } else {
                            // OR between different tables => global predicate
                            global.push(ce.clone())
                        }
                    }
                }

                join.extend(new_join);
                params.extend(new_params);
            } else if is_predicate(op) {
                // atomic selection predicate
                match **rhs {
                    // right-hand side is a column, so this could be a comma join
                    // or a security policy using UserContext
                    Expression::Column(ref rf) => {
                        match **lhs {
                            // column/column comparison
                            Expression::Column(ref lf)
                                if lf.table.is_some()
                                    && tables.contains(&Table::from(
                                        lf.table.as_ref().unwrap().as_str(),
                                    ))
                                    && rf.table.is_some()
                                    && tables.contains(&Table::from(
                                        rf.table.as_ref().unwrap().as_str(),
                                    ))
                                    && lf.table != rf.table =>
                            {
                                // both columns' tables appear in table list and the tables are
                                // different --> comma join
                                if *op == BinaryOperator::Equal {
                                    // equi-join between two tables
                                    let mut jp = JoinPredicate {
                                        left: (**lhs).clone(),
                                        right: (**rhs).clone(),
                                    };
                                    if let Ordering::Less =
                                        rf.table.as_ref().cmp(&lf.table.as_ref())
                                    {
                                        mem::swap(&mut jp.left, &mut jp.right);
                                    }
                                    join.push(jp);
                                } else {
                                    // non-equi-join?
                                    unsupported!("non-equi-join?");
                                }
                            }
                            _ => {
                                // not a comma join, just an ordinary comparison with a
                                // computed column. This must be a global predicate because it
                                // crosses "tables" (the computed column has no associated
                                // table)
                                global.push(ce.clone());
                            }
                        }
                    }
                    // right-hand side is a placeholder, so this must be a query parameter
                    // We carry placeholder numbers all the way to reader nodes so that they can be
                    // mapped to a reader key column
                    Expression::Literal(Literal::Placeholder(ref placeholder)) => {
                        if let Expression::Column(ref lf) = **lhs {
                            let idx = match placeholder {
                                ItemPlaceholder::DollarNumber(idx) => Some(*idx as usize),
                                _ => None,
                            };
                            params.push(Parameter {
                                col: lf.clone(),
                                op: *op,
                                placeholder_idx: idx,
                            });
                        }
                    }
                    // right-hand side is a non-placeholder literal, so this is a predicate
                    Expression::Literal(_) => {
                        if let Expression::Column(ref lf) = **lhs {
                            // we assume that implied table names have previously been expanded
                            // and thus all non-computed columns carry table names
                            if lf.table.is_some() {
                                let e = local.entry(lf.table.clone().unwrap()).or_default();
                                e.push(ce.clone());
                            } else {
                                // comparisons between computed columns and literals are global
                                // predicates
                                global.push(ce.clone());
                            }
                        }
                    }
                    Expression::NestedSelect(_) => {
                        unsupported!("nested SELECTs are unsupported")
                    }
                    Expression::Call(_)
                    | Expression::BinaryOp { .. }
                    | Expression::UnaryOp { .. }
                    | Expression::CaseWhen { .. }
                    | Expression::Exists(_)
                    | Expression::Between { .. }
                    | Expression::Cast { .. }
                    | Expression::In { .. }
                    | Expression::Variable(_) => {
                        unsupported!(
                            "Unsupported right-hand side of condition expression: {}",
                            rhs
                        )
                    }
                }
            } else {
                unsupported!("Arithmetic not supported here")
            }
        }
        Expression::In {
            lhs,
            rhs: InValue::List(rhs),
            ..
        } => {
            let tables = lhs
                .referred_columns()
                .chain(rhs.iter().flat_map(|expr| expr.referred_columns()))
                .flat_map(|col| &col.table)
                .collect::<HashSet<_>>();
            let num_tables = tables.len();
            match tables.into_iter().next() {
                // TODO(grfn): This limitation probably isn't too hard to lift
                None => {
                    unsupported!("Filter conditions must currently mention at least one column")
                }
                Some(table) if num_tables == 1 => {
                    // only one table mentioned, so local
                    local.entry(table.clone()).or_default().push(ce.clone())
                }
                _ => {
                    // more than 1 table mentioned, so must be a global predicate
                    global.push(ce.clone())
                }
            }
        }
        Expression::UnaryOp {
            op: UnaryOperator::Not,
            ..
        } => {
            internal!("negation should have been removed earlier");
        }
        Expression::Exists(_) => unsupported!("EXISTS not supported yet"),
        Expression::Between { .. } => {
            internal!("Between should have been removed earlier")
        }
        expr => {
            // don't expect to see a base here: we ought to exit when classifying its
            // parent selection predicate
            internal!(
                "encountered unexpected standalone base of condition expression {:?}",
                expr
            );
        }
    }
    Ok(())
}

/// Convert the given `Expression`, which should be a set of AND-ed together direct
/// comparison predicates, into a list of predicate expressions
fn collect_join_predicates(cond: Expression, out: &mut Vec<JoinPredicate>) -> ReadySetResult<()> {
    match cond {
        Expression::BinaryOp {
            op: BinaryOperator::Equal,
            lhs,
            rhs,
        } => {
            out.push(JoinPredicate {
                left: *lhs,
                right: *rhs,
            });
            Ok(())
        }
        Expression::BinaryOp {
            lhs,
            op: BinaryOperator::And,
            rhs,
        } => {
            collect_join_predicates(*lhs, out)?;
            collect_join_predicates(*rhs, out)?;
            Ok(())
        }
        _ => {
            unsupported!("Only direct comparisons combined with AND supported for join conditions")
        }
    }
}

#[allow(clippy::cognitive_complexity)]
pub fn to_query_graph(st: &SelectStatement) -> ReadySetResult<QueryGraph> {
    let mut qg = QueryGraph::new();

    if st.tables.is_empty() {
        unsupported!("SELECT statements with no tables are unsupported")
    }

    // a handy closure for making new relation nodes
    let new_node = |rel: String,
                    preds: Vec<Expression>,
                    st: &SelectStatement|
     -> ReadySetResult<QueryGraphNode> {
        Ok(QueryGraphNode {
            rel_name: rel.clone(),
            predicates: preds,
            columns: st
                .fields
                .iter()
                .map(|field| {
                    Ok(match field {
                        // unreachable because SQL rewrite passes will have expanded these already
                        FieldDefinitionExpression::All => {
                            internal!("* should have been expanded already")
                        }
                        FieldDefinitionExpression::AllInTable(_) => {
                            internal!("<table>.* should have been expanded already")
                        }
                        FieldDefinitionExpression::Expression {
                            expr: Expression::Column(c),
                            ..
                        } => match c.table.as_ref() {
                            None => internal!("No table name set for column {} on {}", c.name, rel),
                            Some(t) => {
                                if *t == rel {
                                    Some(c.clone())
                                } else {
                                    None
                                }
                            }
                        },
                        FieldDefinitionExpression::Expression { .. } => {
                            // No need to do anything for expressions here, since they aren't associated
                            // with a relation (and thus have no QGN)
                            // XXX(malte): don't drop aggregation columns
                            None
                        }
                    })
                })
                // FIXME(eta): error handling overhead
                .collect::<ReadySetResult<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect(),
            parameters: Vec::new(),
        })
    };

    // 1. Add any relations mentioned in the query to the query graph.
    // This is needed so that we don't end up with an empty query graph when there are no
    // conditionals, but rather with a one-node query graph that has no predicates.
    for table in &st.tables {
        qg.relations.insert(
            table.name.clone(),
            new_node(table.name.clone(), Vec::new(), st)?,
        );
    }
    for jc in &st.join {
        match jc.right {
            JoinRightSide::Table(ref table) => {
                if !qg.relations.contains_key(&table.name) {
                    qg.relations.insert(
                        table.name.clone(),
                        new_node(table.name.clone(), Vec::new(), st)?,
                    );
                }
            }
            _ => unsupported!("only tables are supported on the RHS of a JOIN"),
        }
    }

    // 2. Add edges for each pair of joined relations. Note that we must keep track of the join
    //    predicates here already, but more may be added when processing the WHERE clause lateron.
    let mut join_predicates = Vec::new();
    let col_expr = |tbl: &str, col: &str| -> Expression {
        Expression::Column(Column {
            table: Some(tbl.into()),
            name: (col.into()),
            function: None,
        })
    };

    // 2a. Explicit joins
    // The table specified in the query is available for USING joins.
    let prev_table = Some(st.tables.last().as_ref().unwrap().name.clone());
    for jc in &st.join {
        match jc.right {
            JoinRightSide::Table(ref table) => {
                // will be defined by join constraint
                let left_table;
                let right_table;

                let join_preds = match &jc.constraint {
                    JoinConstraint::On(cond) => {
                        use nom_sql::analysis::ReferredTables;

                        // find all distinct tables mentioned in the condition
                        // conditions for now.
                        let mut tables_mentioned: Vec<String> =
                            cond.referred_tables().into_iter().map(|t| t.name).collect();

                        let mut join_preds = vec![];
                        collect_join_predicates(cond.clone(), &mut join_preds)?;

                        if tables_mentioned.len() == 2 {
                            // tables can appear in any order in the join predicate, but
                            // we cannot just rely on that order, since it may lead us to
                            // flip LEFT JOINs by accident (yes, this happened)
                            if tables_mentioned[1] != table.name {
                                // tables are in the wrong order in join predicate, swap
                                tables_mentioned.swap(0, 1);
                                invariant_eq!(tables_mentioned[1], table.name);
                            }
                            left_table = tables_mentioned.remove(0);
                            right_table = tables_mentioned.remove(0);
                        } else if tables_mentioned.len() == 1 {
                            // just one table mentioned --> this is a self-join
                            left_table = tables_mentioned.remove(0);
                            right_table = left_table.clone();
                        } else {
                            unsupported!("more than 2 tables mentioned in join condition!");
                        };

                        for pred in join_preds.iter_mut() {
                            // the condition tree might specify tables in opposite order to
                            // their join order in the query; if so, flip them
                            // TODO(malte): this only deals with simple, flat join
                            // conditions for now.
                            let l = match &pred.left {
                                Expression::Column(f) => f,
                                ref x => unsupported!("join condition not supported: {:?}", x),
                            };
                            let r = match &pred.right {
                                Expression::Column(f) => f,
                                ref x => unsupported!("join condition not supported: {:?}", x),
                            };
                            if *l.table.as_ref().unwrap() == right_table
                                && *r.table.as_ref().unwrap() == left_table
                            {
                                mem::swap(&mut pred.left, &mut pred.right);
                            }
                        }

                        join_preds
                    }
                    JoinConstraint::Using(cols) => {
                        invariant_eq!(cols.len(), 1);
                        let col = cols.iter().next().unwrap();

                        left_table = prev_table.as_ref().unwrap().clone();
                        right_table = table.name.clone();

                        vec![JoinPredicate {
                            left: col_expr(&left_table, &col.name),
                            right: col_expr(&right_table, &col.name),
                        }]
                    }
                    JoinConstraint::Empty => {
                        left_table = prev_table.as_ref().unwrap().clone();
                        right_table = table.name.clone();
                        // An empty predicate indicates a cartesian product is expected
                        vec![]
                    }
                };

                // add edge for join
                // FIXME(eta): inefficient cloning!
                if let std::collections::hash_map::Entry::Vacant(e) =
                    qg.edges.entry((left_table.clone(), right_table.clone()))
                {
                    e.insert(match jc.operator {
                        JoinOperator::LeftJoin | JoinOperator::LeftOuterJoin => {
                            QueryGraphEdge::LeftJoin { on: join_preds }
                        }
                        JoinOperator::Join | JoinOperator::InnerJoin => {
                            QueryGraphEdge::Join { on: join_preds }
                        }
                        _ => unsupported!("join operator not supported"),
                    });
                }
            }
            _ => internal!(),
        }
    }

    if let Some(ref cond) = st.where_clause {
        let mut local_predicates = HashMap::new();
        let mut global_predicates = Vec::new();
        let mut query_parameters = Vec::new();
        // Let's classify the predicates we have in the query
        classify_conditionals(
            cond,
            &st.tables,
            &mut local_predicates,
            &mut join_predicates,
            &mut global_predicates,
            &mut query_parameters,
        )?;

        for (_, ces) in local_predicates.iter_mut() {
            *ces = split_conjunctions(ces.clone());
        }

        // 1. Add local predicates for each node that has them
        for (rel, preds) in local_predicates {
            if !qg.relations.contains_key(&rel) {
                // can't have predicates on tables that do not appear in the FROM part of the
                // statement
                internal!(
                    "predicate(s) {:?} on relation {} that is not in query graph",
                    preds,
                    rel
                );
            } else {
                qg.relations.get_mut(&rel).unwrap().predicates.extend(preds);
            }
        }

        // 2. Add predicates for implied (comma) joins
        for jp in join_predicates {
            if let Expression::Column(l) = &jp.left {
                if let Expression::Column(r) = &jp.right {
                    let nn = new_node(l.table.clone().unwrap(), Vec::new(), st)?;
                    // If tables aren't already in the relations, add them.
                    qg.relations
                        .entry(l.table.clone().unwrap())
                        .or_insert_with(|| nn.clone());

                    qg.relations
                        .entry(r.table.clone().unwrap())
                        .or_insert_with(|| nn.clone());

                    let e = qg
                        .edges
                        .entry((l.table.clone().unwrap(), r.table.clone().unwrap()))
                        .or_insert_with(|| QueryGraphEdge::Join { on: vec![] });
                    match *e {
                        QueryGraphEdge::Join { on: ref mut preds } => preds.push(jp.clone()),
                        _ => internal!("Expected join edge for join condition {:#?}", jp),
                    };
                }
            }
        }

        // 3. Add any columns that are query parameters, and which therefore must appear in the leaf
        //    node for this query. Such columns will be carried all the way through the operators
        //    implementing the query (unlike in a traditional query plan, where the predicates on
        //    parameters might be evaluated sooner).
        for param in query_parameters.into_iter() {
            match param.col.table {
                None => {
                    unsupported!("each parameter's column must have an associated table! (no such column \"{}\")", param.col);
                }
                Some(ref table) => {
                    let rel = qg.relations.get_mut(table).unwrap();
                    if !rel.columns.contains(&param.col) {
                        rel.columns.push(param.col.clone());
                    }
                    // the parameter column is included in the projected columns of the output, but
                    // we also separately register it as a parameter so that we can set keys
                    // correctly on the leaf view
                    rel.parameters.push(param.clone());
                }
            }
        }

        // 4. Add global predicates
        qg.global_predicates = global_predicates;
    }

    // 4. Add query graph nodes for any computed columns, which won't be represented in the
    //    nodes corresponding to individual relations.
    let add_computed_column = |qg: &mut QueryGraph, function, name| {
        let column = Column {
            name,
            table: None,
            function: Some(Box::new(function)),
        };

        // add a special node representing the computed columns; if it already
        // exists, add another computed column to it
        let n = qg
            .relations
            .entry("computed_columns".to_owned())
            .or_insert_with(|| new_node("computed_columns".to_owned(), vec![], st).unwrap());
        n.columns.push(column.clone());
        column
    };

    for field in st.fields.iter() {
        match field {
            FieldDefinitionExpression::All | FieldDefinitionExpression::AllInTable(_) => {
                internal!("Stars should have been expanded by now!")
            }
            FieldDefinitionExpression::Expression { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| expr.to_string());
                match expr {
                    Expression::Literal(l) => {
                        qg.columns.push(OutputColumn::Literal(LiteralColumn {
                            name,
                            table: None,
                            value: l.clone(),
                        }))
                    }
                    Expression::Column(c) => {
                        qg.columns.push(OutputColumn::Data {
                            alias: alias.clone().unwrap_or_else(|| c.name.clone()),
                            column: c.clone(),
                        });
                    }
                    Expression::Call(function) if is_aggregate(function) => {
                        let column = add_computed_column(&mut qg, function.clone(), name.clone());
                        qg.columns.push(OutputColumn::Data {
                            alias: name,
                            column,
                        })
                    }
                    _ => {
                        let mut expr = expr.clone();
                        let aggs = map_aggregates(&mut expr);
                        for (agg, name) in aggs {
                            add_computed_column(&mut qg, agg, name);
                        }

                        qg.columns.push(OutputColumn::Expression(ExpressionColumn {
                            name,
                            table: None,
                            expression: expr,
                        }))
                    }
                };
            }
        }
    }

    match st.group_by {
        None => (),
        Some(ref clause) => {
            for column in &clause.columns {
                // add an edge for each relation whose columns appear in the GROUP BY clause
                let e = qg
                    .edges
                    .entry((
                        String::from("computed_columns"),
                        column.table.as_ref().unwrap().clone(),
                    ))
                    .or_insert_with(|| QueryGraphEdge::GroupBy(vec![]));
                match *e {
                    QueryGraphEdge::GroupBy(ref mut cols) => cols.push(column.clone()),
                    _ => internal!(),
                }
            }
        }
    }

    if let Some(ref order) = st.order {
        // For each column in the `ORDER BY` clause, check if it needs to be projected
        order
            .columns
            .iter()
            .for_each(|(ord_col, _)| match (&ord_col.table, &ord_col.function) {
                (None, None) => {
                    // This is a reference to a projected column, otherwise the table value
                    // would be assigned in the `rewrite_selection` pass
                }
                (Some(_), None) => {
                    // This is a reference to a column in a table, we need to project it if it is not
                    // yet projected in order to be able to execute `ORDER BY` post lookup.
                    if !qg.columns.iter().any(
                        |e| matches!(e, OutputColumn::Data {  column, .. } if column == ord_col),
                    ) {
                        // The projected column does not already contains that column, so add it
                        qg.columns.push(OutputColumn::Data {
                            alias: ord_col.name.clone(),
                            column: ord_col.clone(),
                        })
                    }
                }
                (_, Some(box func)) => {
                    // This is a function call expression that we need to add to the list of projected columns
                    let column = add_computed_column(&mut qg, func.clone(), ord_col.name.clone());
                    qg.columns.push(OutputColumn::Data {
                        alias: column.name.clone(),
                        column,
                    })
                }
            });

        qg.pagination = Some(Pagination {
            order: order.columns.clone(),
            limit: st.limit.as_ref().map(|lim| lim.limit.clone()),
            offset: st
                .limit
                .as_ref()
                .and_then(|lim| lim.offset.as_ref())
                .cloned(),
        })
    }

    // create initial join order
    {
        let mut sorted_edges: Vec<(&(String, String), &QueryGraphEdge)> = qg.edges.iter().collect();
        // Sort the edges to ensure deterministic join order.
        sorted_edges.sort_by(|&(a, _), &(b, _)| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

        for ((src, dst), edge) in sorted_edges {
            match edge {
                QueryGraphEdge::Join { .. } | QueryGraphEdge::LeftJoin { .. } => {
                    qg.join_order.push(JoinRef {
                        src: src.clone(),
                        dst: dst.clone(),
                    })
                }
                QueryGraphEdge::GroupBy(_) => continue,
            }
        }
    }

    Ok(qg)
}
