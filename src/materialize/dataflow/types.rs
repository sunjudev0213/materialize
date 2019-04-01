// Copyright 2019 Timely Data, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Timely Data, Inc.

use serde::{Deserialize, Serialize};

use crate::repr::{Datum, Type};

/// System-wide notion of time.
pub type Time = std::time::Duration;

/// System-wide update type.
pub type Diff = isize;

/// A named stream of data.
#[serde(rename_all = "snake_case")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Dataflow {
    Source(Source),
    /// A view is a named transformation from one dataflow to another.
    View(View),
}

impl Dataflow {
    pub fn name(&self) -> &str {
        match self {
            Dataflow::Source(src) => &src.name,
            Dataflow::View(view) => &view.name,
        }
    }

    pub fn typ(&self) -> &Type {
        match self {
            Dataflow::Source(src) => &src.typ,
            Dataflow::View(view) => &view.typ,
        }
    }
}

/// A data source materializes data. It typically represents an external source
/// of data, like a topic from Apache Kafka.
#[serde(rename_all = "snake_case")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub name: String,
    pub connector: Connector,
    pub typ: Type,
    pub raw_schema: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Connector {
    Kafka {
        addr: std::net::SocketAddr,
        topic: String,
    },
}

/// A view transforms one dataflow into another.
#[serde(rename_all = "snake_case")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub name: String,
    pub plan: Plan,
    pub typ: Type,
}

#[serde(rename_all = "snake_case")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Plan {
    /// Source data from another dataflow.
    Source(String),
    /// Project or permute the columns in a dataflow.
    Project {
        outputs: Vec<Expr>,
        /// Plan for the input.
        input: Box<Plan>,
    },
    /// Suppress duplicate tuples.
    Distinct(Box<Plan>),
    /// Union several dataflows of the same type.
    UnionAll(Vec<Plan>),
    /// Join two dataflows.
    Join {
        /// Pairs of indices whose values must be equal.
        keys: Vec<(usize, usize)>,
        /// Plan for the left input.
        left: Box<Plan>,
        /// Plan for the right input.
        right: Box<Plan>,
    },
}

#[serde(rename_all = "snake_case")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    Column(usize),
    Literal(Datum),
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use std::error::Error;

    use super::*;
    use crate::repr::{FType, Type};

    /// Verify that a basic plan serializes and deserializes to JSON sensibly.
    #[test]
    fn test_roundtrip() -> Result<(), Box<dyn Error>> {
        let dataflow = Dataflow::View(View {
            name: "report".into(),
            plan: Plan::Project {
                outputs: vec![Expr::Column(1), Expr::Column(2)],
                input: Box::new(Plan::Join {
                    keys: vec![(0, 0)],
                    left: Box::new(Plan::Source("orders".into())),
                    right: Box::new(Plan::Distinct(Box::new(Plan::UnionAll(vec![
                        Plan::Source("customers2018".into()),
                        Plan::Source("customers2019".into()),
                    ])))),
                }),
            },
            typ: Type {
                name: None,
                nullable: false,
                ftype: FType::Tuple(vec![
                    Type {
                        name: Some("name".into()),
                        nullable: false,
                        ftype: FType::String,
                    },
                    Type {
                        name: Some("quantity".into()),
                        nullable: false,
                        ftype: FType::Int32,
                    },
                ]),
            },
        });

        let encoded = r#"{
  "view": {
    "name": "report",
    "plan": {
      "project": {
        "outputs": [
          {
            "column": 1
          },
          {
            "column": 2
          }
        ],
        "input": {
          "join": {
            "keys": [
              [
                0,
                0
              ]
            ],
            "left": {
              "source": "orders"
            },
            "right": {
              "distinct": {
                "union_all": [
                  {
                    "source": "customers2018"
                  },
                  {
                    "source": "customers2019"
                  }
                ]
              }
            }
          }
        }
      }
    },
    "typ": {
      "name": null,
      "nullable": false,
      "ftype": {
        "tuple": [
          {
            "name": "name",
            "nullable": false,
            "ftype": "string"
          },
          {
            "name": "quantity",
            "nullable": false,
            "ftype": "int32"
          }
        ]
      }
    }
  }
}"#;
        assert_eq!(encoded, &serde_json::to_string_pretty(&dataflow)?);

        let decoded: Dataflow = serde_json::from_str(encoded)?;
        assert_eq!(decoded, dataflow);

        Ok(())
    }
}
