// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::HashSet;

use anyhow::{anyhow, bail, Context};

use ordered_float::OrderedFloat;
use protobuf::descriptor::FileDescriptorSet;
use protobuf::Message;
use serde::de::Deserialize;
use serde_protobuf::de::Deserializer;
use serde_protobuf::descriptor::{
    Descriptors, FieldDescriptor, FieldLabel, FieldType, MessageDescriptor,
};
use serde_protobuf::value::Value as ProtoValue;
use serde_value::Value as SerdeValue;

use ore::str::StrExt;
use repr::adt::numeric::Numeric;
use repr::{ColumnName, ColumnType, Datum, DatumList, Row, ScalarType};

/// A decoded description of the schema of a Protobuf message.
#[derive(Debug)]
pub struct DecodedDescriptors {
    descriptors: Descriptors,
    message_name: String,
    columns: Vec<(ColumnName, ColumnType)>,
}

impl DecodedDescriptors {
    /// Builds a `DecodedDescriptors` from an encoded [`FileDescriptorSet`]
    /// and the fully qualified name of a message inside that file descriptor
    /// set.
    pub fn from_bytes(bytes: &[u8], message_name: String) -> Result<Self, anyhow::Error> {
        let fds =
            FileDescriptorSet::parse_from_bytes(bytes).context("parsing file descriptor set")?;
        let descriptors = Descriptors::from_proto(&fds);

        let message = descriptors.message_by_name(&message_name).ok_or_else(|| {
            // TODO(benesch): the error message here used to include the names of
            // all messages in the descriptor set, but that one feature required
            // maintaining a fork of serde_protobuf. I sent the patch upstream [0],
            // and we can add the error message improvement back if that patch is
            // accepted.
            // [0]: https://github.com/dflemstr/serde-protobuf/pull/9
            anyhow!(
                "Message {} not found in file descriptor set",
                message_name.quoted()
            )
        })?;
        let mut seen_messages = HashSet::new();
        seen_messages.insert(message.name());
        let mut columns = vec![];
        for field in message.fields() {
            let name = ColumnName::from(field.name());
            let ty = derive_column_type(&mut seen_messages, &field, &descriptors)?;
            columns.push((name, ty))
        }

        Ok(DecodedDescriptors {
            descriptors,
            message_name,
            columns,
        })
    }

    /// Describes the columns in the message.
    ///
    /// In other words, the return value describes the shape of the rows that
    /// will be produced by a [`Decoder`] constructed from this
    /// `DecodedDescriptors`.
    pub fn columns(&self) -> &[(ColumnName, ColumnType)] {
        &self.columns
    }

    fn message_descriptor(&self) -> &MessageDescriptor {
        self.descriptors
            .message_by_name(&self.message_name)
            .expect("message validated to exist")
    }
}

/// Decodes a particular Protobuf message from its wire format.
#[derive(Debug)]
pub struct Decoder {
    descriptors: DecodedDescriptors,
    packer: Row,
}

impl Decoder {
    /// Constructs a decoder for a particular Protobuf message.
    pub fn new(descriptors: DecodedDescriptors) -> Self {
        Decoder {
            descriptors,
            packer: Row::default(),
        }
    }

    /// Decodes the encoded Protobuf message into a [`Row`].
    pub fn decode(&mut self, bytes: &[u8]) -> Result<Option<Row>, anyhow::Error> {
        let message = self.descriptors.message_descriptor();
        let input_stream = protobuf::CodedInputStream::from_bytes(bytes);
        let mut deserializer =
            Deserializer::new(&self.descriptors.descriptors, message, input_stream);
        let deserialized_message =
            SerdeValue::deserialize(&mut deserializer).context("Deserializing into rust object")?;

        let deserialized_message = match deserialized_message {
            SerdeValue::Map(deserialized_message) => deserialized_message,
            _ => bail!("Deserialization failed with an unsupported top level object type"),
        };

        for (f, (_name, ty)) in message.fields().iter().zip(self.descriptors.columns()) {
            let key = SerdeValue::String(f.name().to_string());
            let value = deserialized_message.get(&key);
            if let Some(value) = value {
                json_from_serde_value(
                    &value,
                    &mut self.packer,
                    f,
                    &self.descriptors.descriptors,
                    ty,
                )?;
            } else {
                self.packer
                    .push(default_datum_from_field(f, &self.descriptors.descriptors)?);
            }
        }

        Ok(Some(self.packer.finish_and_reuse()))
    }
}

/// Convert an arbitrary [`SerdeValue`] into a [`Datum`], possibly creating a jsonb value
///
/// Top-level values are converted to equivalent Datums, but in the case of a nested
/// type, all numeric types will be converted to f64s (issue #1476)
fn json_from_serde_value(
    val: &SerdeValue,
    packer: &mut Row,
    f: &FieldDescriptor,
    descriptors: &Descriptors,
    column_type: &ColumnType,
) -> Result<(), anyhow::Error> {
    packer.push(match val {
        SerdeValue::Bool(true) => Datum::True,
        SerdeValue::Bool(false) => Datum::False,
        SerdeValue::I8(i) => Datum::Int32(*i as i32),
        SerdeValue::I16(i) => Datum::Int32(*i as i32),
        SerdeValue::I32(i) => Datum::Int32(*i),
        SerdeValue::I64(i) => Datum::Int64(*i),
        SerdeValue::U8(i) => Datum::Int32(*i as i32),
        SerdeValue::U16(i) => Datum::Int32(*i as i32),
        SerdeValue::U32(u) => Datum::from(Numeric::from(*u)),
        SerdeValue::U64(u) => Datum::from(Numeric::from(*u)),
        SerdeValue::F32(f) => Datum::Float32((*f).into()),
        SerdeValue::F64(f) => Datum::Float64((*f).into()),
        SerdeValue::String(s) => Datum::String(s),
        SerdeValue::Bytes(b) => Datum::Bytes(b),
        SerdeValue::Option(s) => {
            if let Some(s) = s {
                return json_from_serde_value(&s, packer, f, descriptors, column_type);
            }

            default_datum_from_field(f, descriptors)?
        }
        SerdeValue::Seq(_) | SerdeValue::Map(_) => {
            return nested_datum_from_serde_value(val, packer, f, descriptors, column_type);
        }
        SerdeValue::Char(_) | SerdeValue::Unit | SerdeValue::Newtype(_) => bail!(
            "Unsupported type for Datum from serde_value::Value: {:?}",
            val
        ),
    });
    Ok(())
}

fn default_datum_from_field<'a>(
    f: &'a FieldDescriptor,
    descriptors: &'a Descriptors,
) -> Result<Datum<'a>, anyhow::Error> {
    if let Some(default) = f.default_value() {
        return datum_from_serde_proto(default);
    }

    if f.is_repeated() {
        return Ok(Datum::List(DatumList::empty()));
    }

    match f.field_type(descriptors) {
        FieldType::Bool => Ok(Datum::False),
        FieldType::Int32 | FieldType::SInt32 | FieldType::SFixed32 => Ok(Datum::Int32(0)),
        FieldType::Int64 | FieldType::SInt64 | FieldType::SFixed64 => Ok(Datum::Int64(0)),
        FieldType::Enum(e) => Ok(Datum::String(
            e.value_by_number(0)
                .expect("Error while deserializing protobuf: expected enum to have zero variant")
                .name(),
        )),
        FieldType::Float => Ok(Datum::Float32(OrderedFloat::from(0.0))),
        FieldType::Double => Ok(Datum::Float64(OrderedFloat::from(0.0))),
        FieldType::UInt32 | FieldType::UInt64 | FieldType::Fixed32 | FieldType::Fixed64 => {
            Ok(Datum::from(Numeric::from(0)))
        }
        FieldType::String => Ok(Datum::String("")),
        FieldType::Bytes => Ok(Datum::Bytes(&[])),
        FieldType::Message(_) => Ok(Datum::Null),
        FieldType::Group => bail!("Unions are currently not supported"),
        FieldType::UnresolvedMessage(m) => bail!("Unresolved message {} not supported", m),
        FieldType::UnresolvedEnum(e) => bail!("Unresolved enum {} not supported", e),
    }
}

fn nested_datum_from_serde_value(
    val: &SerdeValue,
    packer: &mut Row,
    f: &FieldDescriptor,
    descriptors: &Descriptors,
    column_type: &ColumnType,
) -> Result<(), anyhow::Error> {
    packer.push(match val {
        SerdeValue::Bool(true) => Datum::True,
        SerdeValue::Bool(false) => Datum::False,
        SerdeValue::I8(i) => Datum::Int32(*i as i32),
        SerdeValue::I16(i) => Datum::Int32(*i as i32),
        SerdeValue::I32(i) => Datum::Int32(*i),
        SerdeValue::I64(i) => Datum::Int64(*i),
        SerdeValue::U8(i) => Datum::Int32(*i as i32),
        SerdeValue::U16(i) => Datum::Int32(*i as i32),
        SerdeValue::U32(u) => Datum::from(Numeric::from(*u)),
        SerdeValue::U64(u) => Datum::from(Numeric::from(*u)),
        SerdeValue::F32(f) => Datum::Float32((*f).into()),
        SerdeValue::F64(f) => Datum::Float64((*f).into()),
        SerdeValue::String(s) => Datum::String(s),
        SerdeValue::Bytes(_) => {
            bail!("We don't currently support arrays or nested messages with bytes")
        }
        SerdeValue::Seq(s) => {
            let inner_column_type = if let ColumnType {
                scalar_type: ScalarType::List { element_type, .. },
                ..
            } = column_type
            {
                ColumnType {
                    scalar_type: *element_type.clone(),
                    nullable: true,
                }
            } else {
                bail!("sequence must be of type list, found {:?}", column_type);
            };
            return packer.push_list_with(|packer| {
                for value in s {
                    nested_datum_from_serde_value(
                        &value,
                        packer,
                        f,
                        descriptors,
                        &inner_column_type,
                    )?;
                }
                Ok(())
            });
        }
        SerdeValue::Option(v) => {
            if let Some(v) = v {
                return nested_datum_from_serde_value(&v, packer, f, descriptors, column_type);
            }

            default_datum_from_field_nested(f, descriptors)?
        }
        SerdeValue::Map(m) => {
            let nested_message_descriptor = f.field_type(descriptors);
            let pack_map = |packer: &mut Row,
                            k: &SerdeValue,
                            v: &SerdeValue,
                            typ: &ColumnType,
                            push_key: bool| {
                match k {
                    SerdeValue::String(s) => {
                        if push_key {
                            packer.push(Datum::String(s.as_str()));
                        }

                        let nested_message_descriptor = match nested_message_descriptor {
                            FieldType::Message(m) => m,
                            _ => bail!("Nested message is the wrong type"),
                        };

                        nested_datum_from_serde_value(
                            &v,
                            packer,
                            nested_message_descriptor
                                .field_by_name(s)
                                .expect("nested message to exist"),
                            descriptors,
                            typ,
                        )?;
                    }
                    _ => bail!("Unrecognized value while trying to parse a nested message"),
                }
                Ok(())
            };

            match &column_type.scalar_type {
                ScalarType::Map { value_type, .. } => {
                    let mut kvs = m.iter().collect::<Vec<_>>();
                    kvs.sort_by(|(k1, _v1), (k2, _v2)| k1.cmp(k2));

                    return packer.push_dict_with(|packer| {
                        let typ = ColumnType {
                            scalar_type: *value_type.clone(),
                            nullable: true,
                        };
                        for (k, v) in kvs {
                            pack_map(packer, k, v, &typ, true)?;
                        }
                        Ok(())
                    });
                }
                ScalarType::Record { fields, .. } => {
                    return packer.push_list_with(|packer| {
                        for (n, typ) in fields {
                            let (k, v) = m
                                .get_key_value(&SerdeValue::String(n.to_string()))
                                .expect("key value pair to exist");
                            pack_map(packer, k, v, &typ, false)?;
                        }
                        Ok(())
                    });
                }
                _ => bail!("Unsupported scalar type for map"),
            }
        }
        _ => bail!("Unsupported types from serde_value"),
    });
    Ok(())
}

fn default_datum_from_field_nested<'a>(
    f: &'a FieldDescriptor,
    descriptors: &'a Descriptors,
) -> Result<Datum<'a>, anyhow::Error> {
    if let Some(default) = f.default_value() {
        return datum_from_serde_proto_nested(default);
    }

    if f.is_repeated() {
        return Ok(Datum::List(DatumList::empty()));
    }

    match f.field_type(descriptors) {
        FieldType::Bool => Ok(Datum::False),
        FieldType::Int32
        | FieldType::SInt32
        | FieldType::SFixed32
        | FieldType::Int64
        | FieldType::SInt64
        | FieldType::SFixed64
        | FieldType::UInt32
        | FieldType::UInt64
        | FieldType::Fixed32
        | FieldType::Fixed64
        | FieldType::Float
        | FieldType::Double => Ok(Datum::Float64(OrderedFloat::from(0.0))),
        FieldType::Enum(e) => Ok(Datum::String(
            e.value_by_number(0)
                .expect("Error while deserializing protobuf: expected enum to have zero variant")
                .name(),
        )),
        FieldType::String => Ok(Datum::String("")),
        FieldType::Message(_) => Ok(Datum::Null),
        FieldType::Bytes => bail!("Nested bytes are not supported"),
        FieldType::Group => bail!("Unions are currently not supported"),
        FieldType::UnresolvedMessage(m) => bail!("Unresolved message {} not supported", m),
        FieldType::UnresolvedEnum(e) => bail!("Unresolved enum {} not supported", e),
    }
}

fn datum_from_serde_proto<'a>(val: &'a ProtoValue) -> Result<Datum<'a>, anyhow::Error> {
    match val {
        ProtoValue::Bool(true) => Ok(Datum::True),
        ProtoValue::Bool(false) => Ok(Datum::False),
        ProtoValue::I32(i) => Ok(Datum::Int32(*i)),
        ProtoValue::I64(i) => Ok(Datum::Int64(*i)),
        ProtoValue::U32(u) => Ok(Datum::from(Numeric::from(*u))),
        ProtoValue::U64(u) => Ok(Datum::from(Numeric::from(*u))),
        ProtoValue::F32(f) => Ok(Datum::Float32((*f).into())),
        ProtoValue::F64(f) => Ok(Datum::Float64((*f).into())),
        ProtoValue::String(s) => Ok(Datum::String(s)),
        ProtoValue::Bytes(b) => Ok(Datum::Bytes(b)),
        _ => bail!("Unsupported type for Datum from serde_protobuf::Value"),
    }
}

fn datum_from_serde_proto_nested<'a>(val: &'a ProtoValue) -> Result<Datum<'a>, anyhow::Error> {
    if let ProtoValue::Bytes(_) = val {
        bail!("Nested bytes are not supported");
    }
    datum_from_serde_proto(val)
}

fn derive_column_type<'a>(
    seen_messages: &mut HashSet<&'a str>,
    field: &'a FieldDescriptor,
    descriptors: &'a Descriptors,
) -> Result<ColumnType, anyhow::Error> {
    let scalar_type = match field.field_type(descriptors) {
        FieldType::Bool => ScalarType::Bool,
        FieldType::Int32 | FieldType::SInt32 | FieldType::SFixed32 => ScalarType::Int32,
        FieldType::Int64 | FieldType::SInt64 | FieldType::SFixed64 => ScalarType::Int64,
        FieldType::Enum(_) => ScalarType::String,
        FieldType::Float => ScalarType::Float32,
        FieldType::Double => ScalarType::Float64,
        FieldType::UInt32 => bail!("Protobuf type \"uint32\" is not supported"),
        FieldType::UInt64 => bail!("Protobuf type \"uint64\" is not supported"),
        FieldType::Fixed32 => bail!("Protobuf type \"fixed32\" is not supported"),
        FieldType::Fixed64 => bail!("Protobuf type \"fixed64\" is not supported"),
        FieldType::String => ScalarType::String,
        FieldType::Bytes => ScalarType::Bytes,
        FieldType::Message(m) => {
            if seen_messages.contains(m.name()) {
                bail!("Recursive types are not supported: {}", m.name());
            }
            seen_messages.insert(m.name());
            let mut fields = Vec::with_capacity(m.fields().len());
            for field in m.fields() {
                let column_name = ColumnName::from(field.name());
                let column_type = derive_column_type(seen_messages, field, descriptors)?;
                fields.push((column_name, column_type))
            }
            seen_messages.remove(m.name());
            ScalarType::Record {
                fields,
                custom_oid: None,
                custom_name: None,
            }
        }
        FieldType::Group => bail!("Unions are currently not supported"),
        FieldType::UnresolvedMessage(m) => bail!("Unresolved message {} not supported", m),
        FieldType::UnresolvedEnum(e) => bail!("Unresolved enum {} not supported", e),
    };

    match field.field_label() {
        FieldLabel::Required => bail!("Required field {} not supported", field.name()),
        FieldLabel::Repeated => Ok(ColumnType {
            nullable: false,
            scalar_type: ScalarType::List {
                element_type: Box::new(scalar_type),
                custom_oid: None,
            },
        }),
        FieldLabel::Optional => Ok(ColumnType {
            nullable: field.default_value().is_none(),
            scalar_type,
        }),
    }
}
