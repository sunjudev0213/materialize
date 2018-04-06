use std::io::Read;

use failure::{err_msg, Error};
use serde_json::{Map, Value};

pub trait MapHelper {
    fn string(&self, key: &str) -> Option<String>;

    fn name(&self) -> Option<String> {
        self.string("name")
    }

    fn doc(&self) -> Option<String> {
        self.string("doc")
    }
}

impl MapHelper for Map<String, Value> {
    fn string(&self, key: &str) -> Option<String> {
        self.get(key)
            .and_then(|v| v.as_str())
            .map(|v| v.to_string())
    }
}

pub fn zig_i32(n: i32, buffer: &mut Vec<u8>) {
    encode_variable(((n << 1) ^ (n >> 31)) as i64, buffer)
}

pub fn zig_i64(n: i64, buffer: &mut Vec<u8>) {
    encode_variable((n << 1) ^ (n >> 31), buffer)
}

pub fn zag_i32<R: Read>(reader: &mut R) -> Result<i32, Error> {
    let i = zag_i64(reader)?;
    if i < i32::min_value() as i64 || i > i32::max_value() as i64 {
        Err(err_msg("int out of range"))
    } else {
        Ok(i as i32)
    }
}

pub fn zag_i64<R: Read>(reader: &mut R) -> Result<i64, Error> {
    let z = decode_variable(reader)?;
    Ok(if z & 0x1 == 0 {
        (z >> 1) as i64
    } else {
        !(z >> 1) as i64
    })
}

fn encode_variable(mut z: i64, buffer: &mut Vec<u8>) {
    loop {
        if z <= 0x7F {
            buffer.push((z & 0x7F) as u8);
            break
        } else {
            buffer.push((0x80 | (z & 0x7F)) as u8);
            z >>= 7;
        }
    }
}

fn decode_variable<R: Read>(reader: &mut R) -> Result<u64, Error> {
    let mut i = 0u64;
    let mut buf = [0u8; 1];

    let mut j = 0;
    loop {
        reader.read_exact(&mut buf[..])?;
        i |= ((buf[0] & 0x7F) as u64) << (j * 7); // TODO overflow
        if (buf[0] >> 7) == 0 {
            break
        } else {
            j += 1;
        }
    }

    Ok(i)
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_zigzag() {
        // assert_eq!(zig_i32(42i32), zig_i64(42i64))
    }
}
