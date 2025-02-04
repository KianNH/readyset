use std::fmt::{self, Display};

use nom::branch::alt;
use nom::bytes::complete::tag_no_case;
use nom::combinator::{map, opt};
use nom::sequence::{terminated, tuple};
use nom_locate::LocatedSpan;
use serde::{Deserialize, Serialize};

use crate::common::statement_terminator;
use crate::whitespace::whitespace1;
use crate::NomSqlResult;

/// EXPLAIN statements
///
/// This is a non-standard ReadySet-specific extension to SQL
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum ExplainStatement {
    /// Print a (maybe simplified) graphviz representation of the current query graph to stdout
    Graphviz { simplified: bool },
    /// Provides metadata about the last statement that was executed.
    LastStatement,
}

impl Display for ExplainStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EXPLAIN ")?;
        match self {
            ExplainStatement::Graphviz { simplified } => {
                if *simplified {
                    write!(f, "SIMPLIFIED ")?;
                }
                write!(f, "GRAPHVIZ;")
            }
            ExplainStatement::LastStatement => write!(f, "LAST STATEMENT;"),
        }
    }
}

fn explain_graphviz(i: LocatedSpan<&[u8]>) -> NomSqlResult<&[u8], ExplainStatement> {
    let (i, simplified) = opt(terminated(tag_no_case("simplified"), whitespace1))(i)?;
    let (i, _) = tag_no_case("graphviz")(i)?;
    Ok((
        i,
        ExplainStatement::Graphviz {
            simplified: simplified.is_some(),
        },
    ))
}

pub(crate) fn explain_statement(i: LocatedSpan<&[u8]>) -> NomSqlResult<&[u8], ExplainStatement> {
    let (i, _) = tag_no_case("explain")(i)?;
    let (i, _) = whitespace1(i)?;
    let (i, stmt) = alt((
        explain_graphviz,
        map(
            tuple((tag_no_case("last"), whitespace1, tag_no_case("statement"))),
            |_| ExplainStatement::LastStatement,
        ),
    ))(i)?;
    let (i, _) = statement_terminator(i)?;
    Ok((i, stmt))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explain_graphviz() {
        assert_eq!(
            explain_statement(LocatedSpan::new(b"explain graphviz;"))
                .unwrap()
                .1,
            ExplainStatement::Graphviz { simplified: false }
        );
    }

    #[test]
    fn explain_last_statement() {
        assert_eq!(
            explain_statement(LocatedSpan::new(b"explain last statement;"))
                .unwrap()
                .1,
            ExplainStatement::LastStatement
        );
    }
}
