// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! # avro
//! **[Apache Avro](https://avro.apache.org/)** is a data serialization system which provides rich
//! data structures and a compact, fast, binary data format.
//!
//! All data in Avro is schematized, as in the following example:
//!
//! ```text
//! {
//!     "type": "record",
//!     "name": "test",
//!     "fields": [
//!         {"name": "a", "type": "long", "default": 42},
//!         {"name": "b", "type": "string"}
//!     ]
//! }
//! ```
//!
//! There are basically two ways of handling Avro data in Rust:
//!
//! * **as Avro-specialized data types** based on an Avro schema;
//! * **as generic Rust serde-compatible types** implementing/deriving `Serialize` and
//! `Deserialize`;
//!
//! **avro** provides a way to read and write both these data representations easily and
//! efficiently.
//!
//! # Installing the library
//!
//!
//! Add to your `Cargo.toml`:
//!
//! ```text
//! [dependencies]
//! avro = "x.y"
//! ```
//!
//! Or in case you want to leverage the **Snappy** codec:
//!
//! ```text
//! [dependencies.avro]
//! version = "x.y"
//! features = ["snappy"]
//! ```
//!
//! # Defining a schema
//!
//! An Avro data cannot exist without an Avro schema. Schemas **must** be used while writing and
//! **can** be used while reading and they carry the information regarding the type of data we are
//! handling. Avro schemas are used for both schema validation and resolution of Avro data.
//!
//! Avro schemas are defined in **JSON** format and can just be parsed out of a raw string:
//!
//! ```
//! use avro::Schema;
//!
//! let raw_schema = r#"
//!     {
//!         "type": "record",
//!         "name": "test",
//!         "fields": [
//!             {"name": "a", "type": "long", "default": 42},
//!             {"name": "b", "type": "string"}
//!         ]
//!     }
//! "#;
//!
//! // if the schema is not valid, this function will return an error
//! let schema = Schema::parse_str(raw_schema).unwrap();
//!
//! // schemas can be printed for debugging
//! println!("{:?}", schema);
//! ```
//!
//! The library provides also a programmatic interface to define schemas without encoding them in
//! JSON (for advanced use), but we highly recommend the JSON interface. Please read the API
//! reference in case you are interested.
//!
//! For more information about schemas and what kind of information you can encapsulate in them,
//! please refer to the appropriate section of the
//! [Avro Specification](https://avro.apache.org/docs/current/spec.html#schemas).
//!
//! # Writing data
//!
//! Once we have defined a schema, we are ready to serialize data in Avro, validating them against
//! the provided schema in the process. As mentioned before, there are two ways of handling Avro
//! data in Rust.
//!
//! **NOTE:** The library also provides a low-level interface for encoding a single datum in Avro
//! bytecode without generating markers and headers (for advanced use), but we highly recommend the
//! `Writer` interface to be totally Avro-compatible. Please read the API reference in case you are
//! interested.
//!
//! ## The avro way
//!
//! Given that the schema we defined above is that of an Avro *Record*, we are going to use the
//! associated type provided by the library to specify the data we want to serialize:
//!
//! ```
//! # use avro::Schema;
//! use avro::types::Record;
//! use avro::Writer;
//! #
//! # let raw_schema = r#"
//! #     {
//! #         "type": "record",
//! #         "name": "test",
//! #         "fields": [
//! #             {"name": "a", "type": "long", "default": 42},
//! #             {"name": "b", "type": "string"}
//! #         ]
//! #     }
//! # "#;
//! # let schema = Schema::parse_str(raw_schema).unwrap();
//! // a writer needs a schema and something to write to
//! let mut writer = Writer::new(&schema, Vec::new());
//!
//! // the Record type models our Record schema
//! let mut record = Record::new(writer.schema()).unwrap();
//! record.put("a", 27i64);
//! record.put("b", "foo");
//!
//! // schema validation happens here
//! writer.append(record).unwrap();
//!
//! // flushing makes sure that all data gets encoded
//! writer.flush().unwrap();
//!
//! // this is how to get back the resulting avro bytecode
//! let encoded = writer.into_inner();
//! ```
//!
//! The vast majority of the times, schemas tend to define a record as a top-level container
//! encapsulating all the values to convert as fields and providing documentation for them, but in
//! case we want to directly define an Avro value, the library offers that capability via the
//! `Value` interface.
//!
//! ```
//! use avro::types::Value;
//!
//! let mut value = Value::String("foo".to_string());
//! ```
//!
//! ## The serde way
//!
//! Given that the schema we defined above is an Avro *Record*, we can directly use a Rust struct
//! deriving `Serialize` to model our data:
//!
//! ```
//! # use avro::Schema;
//! # use serde::Serialize;
//! use avro::Writer;
//!
//! #[derive(Debug, Serialize)]
//! struct Test {
//!     a: i64,
//!     b: String,
//! }
//!
//! # let raw_schema = r#"
//! #     {
//! #         "type": "record",
//! #         "name": "test",
//! #         "fields": [
//! #             {"name": "a", "type": "long", "default": 42},
//! #             {"name": "b", "type": "string"}
//! #         ]
//! #     }
//! # "#;
//! # let schema = Schema::parse_str(raw_schema).unwrap();
//! // a writer needs a schema and something to write to
//! let mut writer = Writer::new(&schema, Vec::new());
//!
//! // the structure models our Record schema
//! let test = Test {
//!     a: 27,
//!     b: "foo".to_owned(),
//! };
//!
//! // schema validation happens here
//! writer.append_ser(test).unwrap();
//!
//! // flushing makes sure that all data gets encoded
//! writer.flush().unwrap();
//!
//! // this is how to get back the resulting avro bytecode
//! let encoded = writer.into_inner();
//! ```
//!
//! The vast majority of the times, schemas tend to define a record as a top-level container
//! encapsulating all the values to convert as fields and providing documentation for them, but in
//! case we want to directly define an Avro value, any type implementing `Serialize` should work.
//!
//! ```
//! let mut value = "foo".to_string();
//! ```
//!
//! ## Using codecs to compress data
//!
//! Avro supports three different compression codecs when encoding data:
//!
//! * **Null**: leaves data uncompressed;
//! * **Deflate**: writes the data block using the deflate algorithm as specified in RFC 1951, and
//! typically implemented using the zlib library. Note that this format (unlike the "zlib format" in
//! RFC 1950) does not have a checksum.
//! * **Snappy**: uses Google's [Snappy](http://google.github.io/snappy/) compression library. Each
//! compressed block is followed by the 4-byte, big-endianCRC32 checksum of the uncompressed data in
//! the block. You must enable the `snappy` feature to use this codec.
//!
//! To specify a codec to use to compress data, just specify it while creating a `Writer`:
//! ```
//! # use avro::Schema;
//! use avro::Writer;
//! use avro::Codec;
//! #
//! # let raw_schema = r#"
//! #     {
//! #         "type": "record",
//! #         "name": "test",
//! #         "fields": [
//! #             {"name": "a", "type": "long", "default": 42},
//! #             {"name": "b", "type": "string"}
//! #         ]
//! #     }
//! # "#;
//! # let schema = Schema::parse_str(raw_schema).unwrap();
//! let mut writer = Writer::with_codec(&schema, Vec::new(), Codec::Deflate);
//! ```
//!
//! # Reading data
//!
//! As far as reading Avro encoded data goes, we can just use the schema encoded with the data to
//! read them. The library will do it automatically for us, as it already does for the compression
//! codec:
//!
//! ```
//!
//! use avro::Reader;
//! # use avro::Schema;
//! # use avro::types::Record;
//! # use avro::Writer;
//! #
//! # let raw_schema = r#"
//! #     {
//! #         "type": "record",
//! #         "name": "test",
//! #         "fields": [
//! #             {"name": "a", "type": "long", "default": 42},
//! #             {"name": "b", "type": "string"}
//! #         ]
//! #     }
//! # "#;
//! # let schema = Schema::parse_str(raw_schema).unwrap();
//! # let mut writer = Writer::new(&schema, Vec::new());
//! # let mut record = Record::new(writer.schema()).unwrap();
//! # record.put("a", 27i64);
//! # record.put("b", "foo");
//! # writer.append(record).unwrap();
//! # writer.flush().unwrap();
//! # let input = writer.into_inner();
//! // reader creation can fail in case the input to read from is not Avro-compatible or malformed
//! let reader = futures::executor::block_on(Reader::new(&input[..])).unwrap();
//! ```
//!
//! In case, instead, we want to specify a different (but compatible) reader schema from the schema
//! the data has been written with, we can just do as the following:
//! ```
//! use avro::Schema;
//! use avro::Reader;
//! # use avro::types::Record;
//! # use avro::Writer;
//! #
//! # let writer_raw_schema = r#"
//! #     {
//! #         "type": "record",
//! #         "name": "test",
//! #         "fields": [
//! #             {"name": "a", "type": "long", "default": 42},
//! #             {"name": "b", "type": "string"}
//! #         ]
//! #     }
//! # "#;
//! # let writer_schema = Schema::parse_str(writer_raw_schema).unwrap();
//! # let mut writer = Writer::new(&writer_schema, Vec::new());
//! # let mut record = Record::new(writer.schema()).unwrap();
//! # record.put("a", 27i64);
//! # record.put("b", "foo");
//! # writer.append(record).unwrap();
//! # writer.flush().unwrap();
//! # let input = writer.into_inner();
//!
//! let reader_raw_schema = r#"
//!     {
//!         "type": "record",
//!         "name": "test",
//!         "fields": [
//!             {"name": "a", "type": "long", "default": 42},
//!             {"name": "b", "type": "string"},
//!             {"name": "c", "type": "long", "default": 43}
//!         ]
//!     }
//! "#;
//!
//! let reader_schema = Schema::parse_str(reader_raw_schema).unwrap();
//!
//! // reader creation can fail in case the input to read from is not Avro-compatible or malformed
//! let reader = futures::executor::block_on(Reader::with_schema(&reader_schema, &input[..])).unwrap();
//! ```
//!
//! The library will also automatically perform schema resolution while reading the data.
//!
//! For more information about schema compatibility and resolution, please refer to the
//! [Avro Specification](https://avro.apache.org/docs/current/spec.html#schemas).
//!
//! As usual, there are two ways to handle Avro data in Rust, as you can see below.
//!
//! **NOTE:** The library also provides a low-level interface for decoding a single datum in Avro
//! bytecode without markers and header (for advanced use), but we highly recommend the `Reader`
//! interface to leverage all Avro features. Please read the API reference in case you are
//! interested.
//!
//!
//! ## The avro way
//!
//! We can just read directly instances of `Value` out of the `Reader` iterator:
//!
//! ```
//! use futures::stream::StreamExt;
//!
//! # use avro::Schema;
//! # use avro::types::Record;
//! # use avro::Writer;
//! use avro::Reader;
//! #
//! # let raw_schema = r#"
//! #     {
//! #         "type": "record",
//! #         "name": "test",
//! #         "fields": [
//! #             {"name": "a", "type": "long", "default": 42},
//! #             {"name": "b", "type": "string"}
//! #         ]
//! #     }
//! # "#;
//! # let schema = Schema::parse_str(raw_schema).unwrap();
//! # let schema = Schema::parse_str(raw_schema).unwrap();
//! # let mut writer = Writer::new(&schema, Vec::new());
//! # let mut record = Record::new(writer.schema()).unwrap();
//! # record.put("a", 27i64);
//! # record.put("b", "foo");
//! # writer.append(record).unwrap();
//! # writer.flush().unwrap();
//! # let input = writer.into_inner();
//! let mut reader = futures::executor::block_on(Reader::new(&input[..])).unwrap().into_stream();
//!
//! // value is a Result  of an Avro Value in case the read operation fails
//! while let Some(value) = futures::executor::block_on(reader.next()) {
//!     println!("{:?}", value.unwrap());
//! }
//!
//! ```
//!
//! ## The serde way
//!
//! Alternatively, we can use a Rust type implementing `Deserialize` and representing our schema to
//! read the data into:
//!
//! ```
//! use futures::stream::StreamExt;
//! # use avro::Schema;
//! # use avro::Writer;
//! # use serde::{Deserialize, Serialize};
//! use avro::Reader;
//! use avro::from_value;
//!
//! # #[derive(Serialize)]
//! #[derive(Debug, Deserialize)]
//! struct Test {
//!     a: i64,
//!     b: String,
//! }
//!
//! # let raw_schema = r#"
//! #     {
//! #         "type": "record",
//! #         "name": "test",
//! #         "fields": [
//! #             {"name": "a", "type": "long", "default": 42},
//! #             {"name": "b", "type": "string"}
//! #         ]
//! #     }
//! # "#;
//! # let schema = Schema::parse_str(raw_schema).unwrap();
//! # let mut writer = Writer::new(&schema, Vec::new());
//! # let test = Test {
//! #     a: 27,
//! #     b: "foo".to_owned(),
//! # };
//! # writer.append_ser(test).unwrap();
//! # writer.flush().unwrap();
//! # let input = writer.into_inner();
//! let mut reader = futures::executor::block_on(Reader::new(&input[..])).unwrap().into_stream();
//!
//! // value is a Result in case the read operation fails
//! while let Some(value) = futures::executor::block_on(reader.next()) {
//!     println!("{:?}", from_value::<Test>(&value.unwrap()));
//! }
//! ```
//!
//! # Putting everything together
//!
//! The following is an example of how to combine everything showed so far and it is meant to be a
//! quick reference of the library interface:
//!
//! ```
//! use futures::stream::StreamExt;
//! use avro::{Codec, Reader, Schema, Writer, from_value, types::Record};
//! use failure::Error;
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Debug, Deserialize, Serialize)]
//! struct Test {
//!     a: i64,
//!     b: String,
//! }
//!
//! fn main() -> Result<(), Error> {
//!     let raw_schema = r#"
//!         {
//!             "type": "record",
//!             "name": "test",
//!             "fields": [
//!                 {"name": "a", "type": "long", "default": 42},
//!                 {"name": "b", "type": "string"}
//!             ]
//!         }
//!     "#;
//!
//!     let schema = Schema::parse_str(raw_schema)?;
//!
//!     println!("{:?}", schema);
//!
//!     let mut writer = Writer::with_codec(&schema, Vec::new(), Codec::Deflate);
//!
//!     let mut record = Record::new(writer.schema()).unwrap();
//!     record.put("a", 27i64);
//!     record.put("b", "foo");
//!
//!     writer.append(record)?;
//!
//!     let test = Test {
//!         a: 27,
//!         b: "foo".to_owned(),
//!     };
//!
//!     writer.append_ser(test)?;
//!
//!     writer.flush()?;
//!
//!     let input = writer.into_inner();
//!     let mut reader = futures::executor::block_on(Reader::with_schema(&schema, &input[..]))?.into_stream();
//!
//!     while let Some(value) = futures::executor::block_on(reader.next()) {
//!         println!("{:?}", from_value::<Test>(&value?));
//!     }
//!     Ok(())
//! }
//! ```

mod codec;
mod de;
mod decode;
mod encode;
mod reader;
mod ser;
mod util;
mod writer;

pub mod schema;
pub mod types;

pub use crate::codec::Codec;
pub use crate::de::from_value;
pub use crate::reader::{from_avro_datum, Reader};
pub use crate::schema::{ParseSchemaError, Schema};
pub use crate::ser::to_value;
pub use crate::types::SchemaResolutionError;
pub use crate::util::{max_allocation_bytes, DecodeError};
pub use crate::writer::{to_avro_datum, ValidationError, Writer};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::Reader;
    use crate::schema::Schema;
    use crate::types::{Record, Value};

    use futures::stream::StreamExt;

    //TODO: move where it fits better
    #[tokio::test]
    async fn test_enum_default() {
        let writer_raw_schema = r#"
            {
                "type": "record",
                "name": "test",
                "fields": [
                    {"name": "a", "type": "long", "default": 42},
                    {"name": "b", "type": "string"}
                ]
            }
        "#;
        let reader_raw_schema = r#"
            {
                "type": "record",
                "name": "test",
                "fields": [
                    {"name": "a", "type": "long", "default": 42},
                    {"name": "b", "type": "string"},
                    {
                        "name": "c",
                        "type": {
                            "type": "enum",
                            "name": "suit",
                            "symbols": ["diamonds", "spades", "clubs", "hearts"]
                        },
                        "default": "spades"
                    }
                ]
            }
        "#;
        let writer_schema = Schema::parse_str(writer_raw_schema).unwrap();
        let reader_schema = Schema::parse_str(reader_raw_schema).unwrap();
        let mut writer = Writer::with_codec(&writer_schema, Vec::new(), Codec::Null);
        let mut record = Record::new(writer.schema()).unwrap();
        record.put("a", 27i64);
        record.put("b", "foo");
        writer.append(record).unwrap();
        writer.flush().unwrap();
        let input = writer.into_inner();
        let mut reader = Reader::with_schema(&reader_schema, &input[..])
            .await
            .unwrap()
            .into_stream();
        assert_eq!(
            reader.next().await.unwrap().unwrap(),
            Value::Record(vec![
                ("a".to_string(), Value::Long(27)),
                ("b".to_string(), Value::String("foo".to_string())),
                ("c".to_string(), Value::Enum(1, "spades".to_string())),
            ])
        );
        assert!(reader.next().await.is_none());
    }

    //TODO: move where it fits better
    #[tokio::test]
    async fn test_enum_string_value() {
        let raw_schema = r#"
            {
                "type": "record",
                "name": "test",
                "fields": [
                    {"name": "a", "type": "long", "default": 42},
                    {"name": "b", "type": "string"},
                    {
                        "name": "c",
                        "type": {
                            "type": "enum",
                            "name": "suit",
                            "symbols": ["diamonds", "spades", "clubs", "hearts"]
                        },
                        "default": "spades"
                    }
                ]
            }
        "#;
        let schema = Schema::parse_str(raw_schema).unwrap();
        let mut writer = Writer::with_codec(&schema, Vec::new(), Codec::Null);
        let mut record = Record::new(writer.schema()).unwrap();
        record.put("a", 27i64);
        record.put("b", "foo");
        record.put("c", "clubs");
        writer.append(record).unwrap();
        writer.flush().unwrap();
        let input = writer.into_inner();
        let mut reader = Reader::with_schema(&schema, &input[..])
            .await
            .unwrap()
            .into_stream();
        assert_eq!(
            reader.next().await.unwrap().unwrap(),
            Value::Record(vec![
                ("a".to_string(), Value::Long(27)),
                ("b".to_string(), Value::String("foo".to_string())),
                ("c".to_string(), Value::Enum(2, "clubs".to_string())),
            ])
        );
        assert!(reader.next().await.is_none());
    }

    //TODO: move where it fits better
    #[tokio::test]
    async fn test_enum_resolution() {
        let writer_raw_schema = r#"
            {
                "type": "record",
                "name": "test",
                "fields": [
                    {"name": "a", "type": "long", "default": 42},
                    {"name": "b", "type": "string"},
                    {
                        "name": "c",
                        "type": {
                            "type": "enum",
                            "name": "suit",
                            "symbols": ["diamonds", "spades", "clubs", "hearts"]
                        },
                        "default": "spades"
                    }
                ]
            }
        "#;
        let reader_raw_schema = r#"
            {
                "type": "record",
                "name": "test",
                "fields": [
                    {"name": "a", "type": "long", "default": 42},
                    {"name": "b", "type": "string"},
                    {
                        "name": "c",
                        "type": {
                            "type": "enum",
                            "name": "suit",
                            "symbols": ["diamonds", "spades", "ninja", "hearts"]
                        },
                        "default": "spades"
                    }
                ]
            }
        "#;
        let writer_schema = Schema::parse_str(writer_raw_schema).unwrap();
        let reader_schema = Schema::parse_str(reader_raw_schema).unwrap();
        let mut writer = Writer::with_codec(&writer_schema, Vec::new(), Codec::Null);
        let mut record = Record::new(writer.schema()).unwrap();
        record.put("a", 27i64);
        record.put("b", "foo");
        record.put("c", "clubs");
        writer.append(record).unwrap();
        writer.flush().unwrap();
        let input = writer.into_inner();
        let mut reader = Reader::with_schema(&reader_schema, &input[..])
            .await
            .unwrap()
            .into_stream();
        assert!(reader.next().await.unwrap().is_err());
        assert!(reader.next().await.is_none());
    }

    //TODO: move where it fits better
    #[tokio::test]
    async fn test_enum_no_reader_schema() {
        let writer_raw_schema = r#"
            {
                "type": "record",
                "name": "test",
                "fields": [
                    {"name": "a", "type": "long", "default": 42},
                    {"name": "b", "type": "string"},
                    {
                        "name": "c",
                        "type": {
                            "type": "enum",
                            "name": "suit",
                            "symbols": ["diamonds", "spades", "clubs", "hearts"]
                        },
                        "default": "spades"
                    }
                ]
            }
        "#;
        let writer_schema = Schema::parse_str(writer_raw_schema).unwrap();
        let mut writer = Writer::with_codec(&writer_schema, Vec::new(), Codec::Null);
        let mut record = Record::new(writer.schema()).unwrap();
        record.put("a", 27i64);
        record.put("b", "foo");
        record.put("c", "clubs");
        writer.append(record).unwrap();
        writer.flush().unwrap();
        let input = writer.into_inner();
        let mut reader = Reader::new(&input[..]).await.unwrap().into_stream();
        assert_eq!(
            reader.next().await.unwrap().unwrap(),
            Value::Record(vec![
                ("a".to_string(), Value::Long(27)),
                ("b".to_string(), Value::String("foo".to_string())),
                ("c".to_string(), Value::Enum(2, "clubs".to_string())),
            ])
        );
    }
    #[tokio::test]
    async fn test_datetime_value() {
        let writer_raw_schema = r#"{
        "type": "record",
        "name": "dttest",
        "fields": [
            {
                "name": "a",
                "type": {
                    "type": "long",
                    "logicalType": "timestamp-micros"
                }
            }
        ]}"#;
        let writer_schema = Schema::parse_str(writer_raw_schema).unwrap();
        let mut writer = Writer::with_codec(&writer_schema, Vec::new(), Codec::Null);
        let mut record = Record::new(writer.schema()).unwrap();
        let dt = chrono::NaiveDateTime::from_timestamp(1_000, 995_000_000);
        record.put("a", types::Value::Timestamp(dt));
        writer.append(record).unwrap();
        writer.flush().unwrap();
        let input = writer.into_inner();
        let mut reader = Reader::new(&input[..]).await.unwrap().into_stream();
        assert_eq!(
            reader.next().await.unwrap().unwrap(),
            Value::Record(vec![("a".to_string(), Value::Timestamp(dt)),])
        );
    }

    #[tokio::test]
    async fn test_illformed_length() {
        let raw_schema = r#"
            {
                "type": "record",
                "name": "test",
                "fields": [
                    {"name": "a", "type": "long", "default": 42},
                    {"name": "b", "type": "string"}
                ]
            }
        "#;

        let schema = Schema::parse_str(raw_schema).unwrap();

        // Would allocated 18446744073709551605 bytes
        let illformed: &[u8] = &[0x3e, 0x15, 0xff, 0x1f, 0x15, 0xff];

        let value = from_avro_datum(&schema, &mut &illformed[..], None).await;
        assert!(value.is_err());
    }
}
