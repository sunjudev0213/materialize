// Copyright 2019 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

//! SQL `Statement`s are the imperative, side-effecting part of SQL.
//!
//! This module turns SQL `Statement`s into `Plan`s - commands which will drive the dataflow layer

use std::iter::FromIterator;
use std::net::{SocketAddr, ToSocketAddrs};

use failure::{bail, ResultExt};
use sqlparser::ast::{
    Ident, ObjectName, ObjectType, Query, SetVariableValue, ShowStatementFilter, SourceSchema,
    Stage, Statement, Value,
};
use url::Url;

use crate::expr::like::build_like_regex_from_string;
use crate::query;
use crate::session::Session;
use crate::store::{Catalog, CatalogItem, RemoveMode};
use crate::Plan;
use dataflow_types::{
    KafkaSinkConnector, KafkaSourceConnector, PeekWhen, RowSetFinishing, Sink, SinkConnector,
    Source, SourceConnector, View,
};
use expr::{ColumnOrder, RelationExpr};
use interchange::avro;
use ore::option::OptionExt;
use repr::{Datum, RelationDesc, Row, ScalarType};

pub fn describe_statement(
    catalog: &Catalog,
    stmt: Statement,
) -> Result<(Option<RelationDesc>, Vec<ScalarType>), failure::Error> {
    Ok(match stmt {
        Statement::CreateSource { .. }
        | Statement::CreateSink { .. }
        | Statement::CreateView { .. }
        | Statement::Drop { .. }
        | Statement::SetVariable { .. }
        | Statement::Tail { .. } => (None, vec![]),

        Statement::CreateSources { .. } => (
            Some(RelationDesc::empty().add_column("Topic", ScalarType::String)),
            vec![],
        ),

        Statement::Explain { stage, .. } => (
            Some(RelationDesc::empty().add_column(
                match stage {
                    Stage::Dataflow => "Dataflow",
                    Stage::Plan => "Plan",
                },
                ScalarType::String,
            )),
            vec![],
        ),

        Statement::ShowCreateView { .. } => (
            Some(
                RelationDesc::empty()
                    .add_column("View", ScalarType::String)
                    .add_column("Create View", ScalarType::String),
            ),
            vec![],
        ),

        Statement::ShowColumns { .. } => (
            Some(
                RelationDesc::empty()
                    .add_column("Field", ScalarType::String)
                    .add_column("Nullable", ScalarType::String)
                    .add_column("Type", ScalarType::String),
            ),
            vec![],
        ),

        Statement::ShowObjects { object_type } => (
            Some(
                RelationDesc::empty()
                    .add_column(object_type_as_plural_str(object_type), ScalarType::String),
            ),
            vec![],
        ),

        Statement::ShowVariable { variable, .. } => {
            if variable.value == unicase::Ascii::new("ALL") {
                (
                    Some(
                        RelationDesc::empty()
                            .add_column("name", ScalarType::String)
                            .add_column("setting", ScalarType::String)
                            .add_column("description", ScalarType::String),
                    ),
                    vec![],
                )
            } else {
                (
                    Some(RelationDesc::empty().add_column(variable.value, ScalarType::String)),
                    vec![],
                )
            }
        }

        Statement::Peek { name, .. } => {
            (Some(catalog.get(&name.to_string())?.desc().clone()), vec![])
        }

        Statement::Query(query) => {
            // TODO(benesch): ideally we'd save `relation_expr` and `finishing`
            // somewhere, so we don't have to reanalyze the whole query when
            // `handle_statement` is called. This will require a complicated
            // dance when bind parameters are implemented, so punting for now.
            let (_relation_expr, desc, _finishing, param_types) =
                query::plan_root_query(catalog, *query)?;
            (Some(desc), param_types)
        }

        _ => bail!("unsupported SQL statement: {:?}", stmt),
    })
}

/// Dispatch from arbitrary [`sqlparser::ast::Statement`]s to specific handle commands
pub fn handle_statement(
    catalog: &Catalog,
    session: &Session,
    stmt: Statement,
) -> Result<Plan, failure::Error> {
    match stmt {
        Statement::Peek { name, immediate } => handle_peek(catalog, name, immediate),
        Statement::Tail { name } => handle_tail(catalog, name),
        Statement::CreateSource { .. }
        | Statement::CreateSink { .. }
        | Statement::CreateView { .. }
        | Statement::CreateSources { .. } => handle_create_dataflow(catalog, stmt),
        Statement::Drop { .. } => handle_drop_dataflow(catalog, stmt),
        Statement::Query(query) => handle_select(catalog, *query),
        Statement::SetVariable {
            local,
            variable,
            value,
        } => handle_set_variable(catalog, local, variable, value),
        Statement::ShowVariable { variable } => handle_show_variable(catalog, session, variable),
        Statement::ShowObjects { object_type: ot } => handle_show_objects(catalog, ot),
        Statement::ShowColumns {
            extended,
            full,
            table_name,
            filter,
        } => handle_show_columns(catalog, extended, full, &table_name, filter.as_ref()),
        Statement::ShowCreateView { view_name } => handle_show_create_view(catalog, view_name),
        Statement::Explain { stage, query } => handle_explain(catalog, stage, *query),

        _ => bail!("unsupported SQL statement: {:?}", stmt),
    }
}

fn handle_set_variable(
    _: &Catalog,
    local: bool,
    variable: Ident,
    value: SetVariableValue,
) -> Result<Plan, failure::Error> {
    if local {
        bail!("SET LOCAL ... is not supported");
    }
    Ok(Plan::SetVariable {
        name: variable.value,
        value: match value {
            SetVariableValue::Literal(Value::SingleQuotedString(s)) => s,
            SetVariableValue::Literal(lit) => lit.to_string(),
            SetVariableValue::Ident(ident) => ident.value,
        },
    })
}

fn handle_show_variable(
    _: &Catalog,
    session: &Session,
    variable: Ident,
) -> Result<Plan, failure::Error> {
    if variable.value == unicase::Ascii::new("ALL") {
        Ok(Plan::SendRows(
            session
                .vars()
                .iter()
                .map(|v| {
                    Row::from_iter(&[
                        Datum::String(v.name()),
                        Datum::String(&v.value()),
                        Datum::String(v.description()),
                    ])
                })
                .collect(),
        ))
    } else {
        let variable = session.get(&variable.value)?;
        Ok(Plan::SendRows(vec![Row::from_iter(&[Datum::String(
            &variable.value(),
        )])]))
    }
}

fn handle_tail(catalog: &Catalog, from: ObjectName) -> Result<Plan, failure::Error> {
    let from = extract_sql_object_name(&from)?;
    let dataflow = catalog.get(&from)?;
    Ok(Plan::Tail(dataflow.clone()))
}

fn handle_show_objects(catalog: &Catalog, object_type: ObjectType) -> Result<Plan, failure::Error> {
    let mut rows: Vec<Row> = catalog
        .iter()
        .filter(|(_k, v)| object_type_matches(object_type, &v))
        .map(|(k, _v)| Row::from_iter(&[Datum::from(k)]))
        .collect();
    rows.sort_unstable();
    Ok(Plan::SendRows(rows))
}

/// Create an immediate result that describes all the columns for the given table
fn handle_show_columns(
    catalog: &Catalog,
    extended: bool,
    full: bool,
    table_name: &ObjectName,
    filter: Option<&ShowStatementFilter>,
) -> Result<Plan, failure::Error> {
    if extended {
        bail!("SHOW EXTENDED COLUMNS is not supported");
    }
    if full {
        bail!("SHOW FULL COLUMNS is not supported");
    }
    if filter.is_some() {
        bail!("SHOW COLUMNS ... { LIKE | WHERE } is not supported");
    }

    let column_descriptions: Vec<_> = catalog
        .get_desc(&table_name.to_string())?
        .iter()
        .map(|(name, typ)| {
            Row::from_iter(&[
                Datum::String(name.mz_as_deref().unwrap_or("?")),
                Datum::String(if typ.nullable { "YES" } else { "NO" }),
                Datum::String(&typ.scalar_type.to_string()),
            ])
        })
        .collect();

    Ok(Plan::SendRows(column_descriptions))
}

fn handle_show_create_view(
    catalog: &Catalog,
    object_name: ObjectName,
) -> Result<Plan, failure::Error> {
    let name = object_name.to_string();
    let raw_sql = if let CatalogItem::View(view) = catalog.get(&name)? {
        &view.raw_sql
    } else {
        bail!("{} is not a view", name);
    };
    Ok(Plan::SendRows(vec![Row::from_iter(&[
        Datum::String(&name),
        Datum::String(&raw_sql),
    ])]))
}

fn handle_create_dataflow(catalog: &Catalog, mut stmt: Statement) -> Result<Plan, failure::Error> {
    match &mut stmt {
        Statement::CreateView {
            name,
            columns,
            query,
            materialized,
            with_options,
        } => {
            if !with_options.is_empty() {
                bail!("WITH options are not yet supported");
            }
            let (mut relation_expr, mut desc, finishing) = handle_query(catalog, *query.clone())?;
            if !finishing.is_trivial() {
                //TODO: materialize#724 - persist finishing information with the view?
                relation_expr = RelationExpr::Project {
                    input: Box::new(RelationExpr::TopK {
                        input: Box::new(relation_expr),
                        group_key: vec![],
                        order_key: finishing.order_by,
                        limit: finishing.limit,
                        offset: finishing.offset,
                    }),
                    outputs: finishing.project,
                }
            }
            *materialized = false; // Normalize for `raw_sql` below.
            let typ = desc.typ();
            if !columns.is_empty() {
                if columns.len() != typ.column_types.len() {
                    bail!(
                        "VIEW definition has {} columns, but query has {} columns",
                        columns.len(),
                        typ.column_types.len()
                    )
                }
                for (i, name) in columns.iter().enumerate() {
                    desc.set_name(i, Some(name.value.to_owned()));
                }
            }
            let view = View {
                name: extract_sql_object_name(name)?,
                raw_sql: stmt.to_string(),
                relation_expr,
                desc,
            };
            Ok(Plan::CreateView(view))
        }
        Statement::CreateSource {
            name,
            url,
            schema,
            with_options,
        } => {
            if !with_options.is_empty() {
                bail!("WITH options are not yet supported");
            }
            let name = extract_sql_object_name(name)?;
            let (addr, topic) = parse_kafka_topic_url(url)?;
            let source = build_source(schema, addr, name, topic)?;
            Ok(Plan::CreateSource(source))
        }
        Statement::CreateSources {
            like,
            url,
            schema_registry,
            with_options,
        } => {
            if !with_options.is_empty() {
                bail!("WITH options are not yet supported");
            }
            // TODO(brennan): This shouldn't be synchronous either (see CreateSource above),
            // but for now we just want it working for demo purposes...
            let schema_registry_url: Url = schema_registry.parse()?;
            let ccsr_client = ccsr::Client::new(schema_registry_url.clone());
            let mut subjects = ccsr_client.list_subjects()?;
            if let Some(value) = like {
                let like_regex = build_like_regex_from_string(value)?;
                subjects.retain(|a| like_regex.is_match(a))
            }

            let names = subjects
                .iter()
                .filter_map(|s| {
                    let parts: Vec<&str> = s.rsplitn(2, '-').collect();
                    if parts.len() == 2 && parts[0] == "value" {
                        let topic_name = parts[1];
                        let sql_name = sanitize_kafka_topic_name(parts[1]);
                        Some((topic_name, sql_name))
                    } else {
                        None
                    }
                })
                .filter(|(_tn, sn)| catalog.try_get(sn).is_none());
            let (addr, topic) = parse_kafka_url(url)?;
            if let Some(s) = topic {
                bail!(
                    "CREATE SOURCES statement should not take a topic path: {}",
                    s
                );
            }
            let sources = names
                .map(|(topic_name, sql_name)| {
                    Ok(build_source(
                        &SourceSchema::Registry(schema_registry.to_owned()),
                        addr,
                        sql_name,
                        topic_name.to_owned(),
                    )?)
                })
                .collect::<Result<Vec<_>, failure::Error>>()?;
            Ok(Plan::CreateSources(sources))
        }
        Statement::CreateSink {
            name,
            from,
            url,
            with_options,
        } => {
            if !with_options.is_empty() {
                bail!("WITH options are not yet supported");
            }
            let name = extract_sql_object_name(name)?;
            let from = extract_sql_object_name(from)?;
            let dataflow = catalog.get(&from)?;
            let (addr, topic) = parse_kafka_topic_url(url)?;
            let sink = Sink {
                name,
                from: (from, dataflow.desc().clone()),
                connector: SinkConnector::Kafka(KafkaSinkConnector {
                    addr,
                    topic,
                    schema_id: 0,
                }),
            };
            Ok(Plan::CreateSink(sink))
        }
        other => bail!("Unsupported statement: {:?}", other),
    }
}

fn handle_drop_dataflow(catalog: &Catalog, stmt: Statement) -> Result<Plan, failure::Error> {
    let (object_type, if_exists, names, cascade) = match stmt {
        Statement::Drop {
            object_type,
            if_exists,
            names,
            cascade,
        } => (object_type, if_exists, names, cascade),
        _ => unreachable!(),
    };
    let names: Vec<String> = Result::from_iter(names.iter().map(extract_sql_object_name))?;
    for name in &names {
        match catalog.get(name) {
            Ok(dataflow) => {
                if !object_type_matches(object_type, dataflow) {
                    bail!("{} is not of type {}", name, object_type);
                }
            }
            Err(e) => {
                if !if_exists {
                    return Err(e);
                }
            }
        }
    }
    // This needs to have heterogenous drops, because cascades could drop multiple types.
    let mode = RemoveMode::from_cascade(cascade);
    let mut to_remove = vec![];
    for name in &names {
        catalog.plan_remove(name, mode, &mut to_remove)?;
    }
    to_remove.sort();
    to_remove.dedup();
    Ok(match object_type {
        ObjectType::Source => Plan::DropItems(to_remove, ObjectType::Source),
        ObjectType::View => Plan::DropItems(to_remove, ObjectType::View),
        _ => bail!("unsupported SQL statement: DROP {}", object_type),
    })
}

fn handle_peek(
    catalog: &Catalog,
    name: ObjectName,
    immediate: bool,
) -> Result<Plan, failure::Error> {
    let name = name.to_string();
    let dataflow = catalog.get(&name)?.clone();
    let typ = dataflow.typ();
    Ok(Plan::Peek {
        source: ::expr::RelationExpr::Get {
            name: dataflow.name().to_owned(),
            typ: typ.clone(),
        },
        when: if immediate {
            PeekWhen::Immediately
        } else {
            PeekWhen::EarliestSource
        },
        finishing: RowSetFinishing {
            filter: vec![],
            offset: 0,
            limit: None,
            order_by: (0..typ.column_types.len())
                .map(|column| ColumnOrder {
                    column,
                    desc: false,
                })
                .collect(),
            project: (0..typ.column_types.len()).collect(),
        },
    })
}

pub fn handle_select(catalog: &Catalog, query: Query) -> Result<Plan, failure::Error> {
    let (relation_expr, _, finishing) = handle_query(catalog, query)?;
    Ok(Plan::Peek {
        source: relation_expr,
        when: PeekWhen::Immediately,
        finishing,
    })
}

pub fn handle_explain(
    catalog: &Catalog,
    stage: Stage,
    query: Query,
) -> Result<Plan, failure::Error> {
    let (relation_expr, _desc, _finishing) = handle_query(catalog, query)?;
    // Previouly we would bail here for ORDER BY and LIMIT; this has been relaxed to silently
    // report the plan without the ORDER BY and LIMIT decorations (which are done in post).
    if stage == Stage::Dataflow {
        Ok(Plan::SendRows(vec![Row::from_iter(&[Datum::String(
            &relation_expr.pretty(),
        )])]))
    } else {
        Ok(Plan::ExplainPlan(relation_expr))
    }
}

/// Plans and decorrelates a `Query`. Like `query::plan_root_query`, but returns
/// an `::expr::RelationExpr`, which cannot include correlated expressions.
fn handle_query(
    catalog: &Catalog,
    query: Query,
) -> Result<(RelationExpr, RelationDesc, RowSetFinishing), failure::Error> {
    let (expr, desc, finishing, _param_types) = query::plan_root_query(catalog, query)?;
    Ok((expr.decorrelate()?, desc, finishing))
}

fn build_source(
    schema: &SourceSchema,
    kafka_addr: SocketAddr,
    name: String,
    topic: String,
) -> Result<Source, failure::Error> {
    let (key_schema, value_schema, schema_registry_url) = match schema {
        // TODO(jldlaughlin): we need a way to pass in primary key information
        // when building a source from a string
        SourceSchema::Raw(schema) => (None, schema.to_owned(), None),

        SourceSchema::Registry(url) => {
            // TODO(benesch): we need to fetch this schema asynchronously to
            // avoid blocking the command processing thread.
            let url: Url = url.parse()?;
            let ccsr_client = ccsr::Client::new(url.clone());

            let value_schema_name = format!("{}-value", topic);
            let value_schema = ccsr_client
                .get_schema_by_subject(&value_schema_name)
                .with_context(|err| {
                    format!(
                        "fetching latest schema for subject '{}' from registry: {}",
                        value_schema_name, err
                    )
                })?;
            let key_schema = ccsr_client
                .get_schema_by_subject(&format!("{}-key", topic))
                .ok();
            (key_schema.map(|s| s.raw), value_schema.raw, Some(url))
        }
    };

    let mut desc = avro::validate_value_schema(&value_schema)?;
    if let Some(key_schema) = key_schema {
        let keys = avro::validate_key_schema(&key_schema, &desc)?;
        desc = desc.add_keys(keys);
    }

    Ok(Source {
        name,
        connector: SourceConnector::Kafka(KafkaSourceConnector {
            addr: kafka_addr,
            topic,
            raw_schema: value_schema,
            schema_registry_url,
        }),
        desc,
    })
}

fn parse_kafka_url(url: &str) -> Result<(SocketAddr, Option<String>), failure::Error> {
    let url: Url = url.parse()?;
    if url.scheme() != "kafka" {
        bail!("only kafka:// sources are supported: {}", url);
    } else if !url.has_host() {
        bail!("source URL missing hostname: {}", url)
    }
    let topic = match url.path_segments() {
        None => None,
        Some(segments) => {
            let segments: Vec<_> = segments.collect();
            if segments.is_empty() {
                None
            } else if segments.len() != 1 {
                bail!("source URL should have at most one path segment: {}", url);
            } else if segments[0].is_empty() {
                None
            } else {
                Some(segments[0].to_owned())
            }
        }
    };
    // We already checked for kafka scheme above, so it's safe to assume port
    // 9092.
    let addr = url
        .with_default_port(|_| Ok(9092))?
        .to_socket_addrs()?
        .next()
        .unwrap();
    Ok((addr, topic))
}

fn parse_kafka_topic_url(url: &str) -> Result<(SocketAddr, String), failure::Error> {
    let (addr, topic) = parse_kafka_url(url)?;
    if let Some(topic) = topic {
        Ok((addr, topic))
    } else {
        bail!("source URL missing topic path: {}")
    }
}

fn sanitize_kafka_topic_name(topic_name: &str) -> String {
    // Kafka topics can contain alphanumerics, dots (.), underscores (_), and
    // hyphens (-), and most Kafka topics contain at least one of these special
    // characters for namespacing, as in "mysql.tbl". Since non-quoted SQL
    // identifiers cannot contain dots or hyphens, if we were to use the topic
    // name directly as the name of the source, it would be impossible to refer
    // to in SQL without quoting. So we replace hyphens and dots with
    // underscores to make a valid SQL identifier.
    //
    // This scheme has the potential for collisions, but if you're creating
    // Kafka topics like "a.b", "a_b", and "a-b", well... that's why we let you
    // manually create sources with custom names using CREATE SOURCE.
    topic_name.replace("-", "_").replace(".", "_")
}

/// Whether a SQL object type can be interpreted as matching the type of the given catalog item.
/// For example, if `v` is a view, `DROP SOURCE v` should not work, since Source and View
/// are non-matching types.
///
/// For now tables are treated as a special kind of source in Materialize, so just
/// allow `TABLE` to refer to either.
fn object_type_matches(object_type: ObjectType, item: &CatalogItem) -> bool {
    match item {
        CatalogItem::Source { .. } => {
            object_type == ObjectType::Source || object_type == ObjectType::Table
        }
        CatalogItem::Sink { .. } => object_type == ObjectType::Sink,
        CatalogItem::View { .. } => object_type == ObjectType::View,
    }
}

fn object_type_as_plural_str(object_type: ObjectType) -> &'static str {
    match object_type {
        ObjectType::Index => "INDEXES",
        ObjectType::Table => "TABLES",
        ObjectType::View => "VIEWS",
        ObjectType::Source => "SOURCES",
        ObjectType::Sink => "SINKS",
    }
}

pub(crate) fn extract_sql_object_name(n: &ObjectName) -> Result<String, failure::Error> {
    if n.0.len() != 1 {
        bail!("qualified names are not yet supported: {}", n.to_string())
    }
    Ok(n.to_string())
}
