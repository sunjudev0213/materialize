use std::collections::HashMap;
use std::mem::transmute;

use chrono::{NaiveDate, NaiveDateTime};
use failure::Error;

use crate::schema::Schema;
use crate::types::Value;
use crate::util::{safe_len, zag_i32, zag_i64, DecodeError};
use futures::future::{BoxFuture, FutureExt};
use tokio::io::{AsyncRead, AsyncReadExt};

#[inline]
async fn decode_long<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Value, Error> {
    zag_i64(reader).await.map(Value::Long)
}

#[inline]
async fn decode_int<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Value, Error> {
    zag_i32(reader).await.map(Value::Int)
}

#[inline]
async fn decode_len<R: AsyncRead + Unpin>(reader: &mut R) -> Result<usize, Error> {
    zag_i64(reader).await.and_then(|len| safe_len(len as usize))
}

/// Decode a `Value` from avro format given its `Schema`.
pub fn decode<'a, R: AsyncRead + Unpin + Send>(
    schema: &'a Schema,
    reader: &'a mut R,
) -> BoxFuture<'a, Result<Value, Error>> {
    async move {
        match *schema {
            Schema::Null => Ok(Value::Null),
            Schema::Boolean => {
                let mut buf = [0u8; 1];
                reader.read_exact(&mut buf[..]).await?;

                match buf[0] {
                    0u8 => Ok(Value::Boolean(false)),
                    1u8 => Ok(Value::Boolean(true)),
                    _ => Err(DecodeError::new("not a bool").into()),
                }
            }
            Schema::Int => decode_int(reader).await,
            Schema::Long => decode_long(reader).await,
            Schema::Float => {
                let mut buf = [0u8; 4];
                reader.read_exact(&mut buf[..]).await?;
                Ok(Value::Float(unsafe { transmute::<[u8; 4], f32>(buf) }))
            }
            Schema::Double => {
                let mut buf = [0u8; 8];
                reader.read_exact(&mut buf[..]).await?;
                Ok(Value::Double(unsafe { transmute::<[u8; 8], f64>(buf) }))
            }
            Schema::Date => match decode_int(reader).await? {
                Value::Int(days) => Ok(Value::Date(
                    NaiveDate::from_ymd(1970, 1, 1)
                        .checked_add_signed(chrono::Duration::days(days.into()))
                        .ok_or_else(|| {
                            DecodeError::new(format!("Invalid num days from epoch: {0}", days))
                        })?,
                )),
                other => Err(
                    DecodeError::new(format!("Not an Int32 input for Date: {:?}", other)).into(),
                ),
            },
            Schema::TimestampMilli => match decode_long(reader).await? {
                Value::Long(millis) => {
                    let seconds = millis / 1_000;
                    let millis = (millis % 1_000) as u32;
                    Ok(Value::Timestamp(
                        NaiveDateTime::from_timestamp_opt(seconds, millis * 1_000_000).ok_or_else(
                            || {
                                DecodeError::new(format!(
                                    "Invalid ms timestamp {}.{}",
                                    seconds, millis
                                ))
                            },
                        )?,
                    ))
                }
                other => Err(DecodeError::new(format!(
                    "Not an Int64 input for Millisecond DateTime: {:?}",
                    other
                ))
                .into()),
            },
            Schema::TimestampMicro => match decode_long(reader).await? {
                Value::Long(micros) => {
                    let seconds = micros / 1_000_000;
                    let micros = (micros % 1_000_000) as u32;
                    Ok(Value::Timestamp(
                        NaiveDateTime::from_timestamp_opt(seconds, micros * 1_000).ok_or_else(
                            || {
                                DecodeError::new(format!(
                                    "Invalid mu timestamp {}.{}",
                                    seconds, micros
                                ))
                            },
                        )?,
                    ))
                }
                other => Err(DecodeError::new(format!(
                    "Not an Int64 input for Microsecond DateTime: {:?}",
                    other
                ))
                .into()),
            },
            Schema::Decimal {
                precision,
                scale,
                fixed_size,
            } => {
                let len = match fixed_size {
                    Some(len) => len,
                    None => decode_len(reader).await?,
                };
                let mut buf = Vec::with_capacity(len);
                unsafe {
                    buf.set_len(len);
                }
                reader.read_exact(&mut buf).await?;
                Ok(Value::Decimal {
                    unscaled: buf,
                    precision,
                    scale,
                })
            }
            Schema::Bytes => {
                let len = decode_len(reader).await?;
                let mut buf = Vec::with_capacity(len);
                unsafe {
                    buf.set_len(len);
                }
                reader.read_exact(&mut buf).await?;
                Ok(Value::Bytes(buf))
            }
            Schema::String => {
                let len = decode_len(reader).await?;
                let mut buf = Vec::with_capacity(len);
                unsafe {
                    buf.set_len(len);
                }
                reader.read_exact(&mut buf).await?;

                String::from_utf8(buf)
                    .map(Value::String)
                    .map_err(|_| DecodeError::new("not a valid utf-8 string").into())
            }
            Schema::Fixed { size, .. } => {
                let mut buf = vec![0u8; size as usize];
                reader.read_exact(&mut buf).await?;
                Ok(Value::Fixed(size, buf))
            }
            Schema::Array(ref inner) => {
                let mut items = Vec::new();

                loop {
                    let len = decode_len(reader).await?;
                    // arrays are 0-terminated, 0i64 is also encoded as 0 in Avro
                    // reading a length of 0 means the end of the array
                    if len == 0 {
                        break;
                    }

                    items.reserve(len as usize);
                    for _ in 0..len {
                        items.push(decode(inner, reader).await?);
                    }
                }

                Ok(Value::Array(items))
            }
            Schema::Map(ref inner) => {
                let mut items = HashMap::new();

                loop {
                    let len = decode_len(reader).await?;
                    // maps are 0-terminated, 0i64 is also encoded as 0 in Avro
                    // reading a length of 0 means the end of the map
                    if len == 0 {
                        break;
                    }

                    items.reserve(len as usize);
                    for _ in 0..len {
                        if let Value::String(key) = decode(&Schema::String, reader).await? {
                            let value = decode(inner, reader).await?;
                            items.insert(key, value);
                        } else {
                            return Err(DecodeError::new("map key is not a string").into());
                        }
                    }
                }

                Ok(Value::Map(items))
            }
            Schema::Union(ref inner) => {
                let index = zag_i64(reader).await?;
                let variants = inner.variants();
                match variants.get(index as usize) {
                    Some(variant) => decode(variant, reader)
                        .await
                        .map(|x| Value::Union(Box::new(x))),
                    None => Err(DecodeError::new("Union index out of bounds").into()),
                }
            }
            Schema::Record { ref fields, .. } => {
                // Benchmarks indicate ~10% improvement using this method.
                let mut items = Vec::new();
                items.reserve(fields.len());
                for field in fields {
                    // This clone is also expensive. See if we can do away with it...
                    items.push((field.name.clone(), decode(&field.schema, reader).await?));
                }
                Ok(Value::Record(items))
                // fields
                // .iter()
                // .map(|field| decode(&field.schema, reader).map(|value| (field.name.clone(), value)))
                // .collect::<Result<Vec<(String, Value)>, _>>()
                // .map(|items| Value::Record(items))
            }
            Schema::Enum { ref symbols, .. } => {
                if let Value::Int(index) = decode_int(reader).await? {
                    if index >= 0 && (index as usize) <= symbols.len() {
                        let symbol = symbols[index as usize].clone();
                        Ok(Value::Enum(index, symbol))
                    } else {
                        Err(DecodeError::new("enum symbol index out of bounds").into())
                    }
                } else {
                    Err(DecodeError::new("enum symbol not found").into())
                }
            }
        }
    }
    .boxed()
}
