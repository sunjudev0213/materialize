// Copyright 2019 Materialize, Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License in the LICENSE file at the
// root of this repository, or online at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use crate::ast::ParsedDateTime;
use crate::parser::{DateTimeField, ParserError};
use std::str::FromStr;

// TimeStrToken represents valid tokens in time-like strings,
// i.e those used in INTERVAL, TIMESTAMP/TZ, DATE, and TIME.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TimeStrToken {
    Dash,
    Space,
    Colon,
    Dot,
    Plus,
    Zulu,
    Num(i64),
    Nanos(i64),
    // String representation of a named timezone e.g. 'EST'
    TzName(String),
    // Tokenized version of a DateTimeField string e.g. 'YEAR'
    TimeUnit(DateTimeField),
}

pub(crate) fn tokenize_time_str(value: &str) -> Result<Vec<TimeStrToken>, ParserError> {
    let mut toks = vec![];
    let mut num_buf = String::with_capacity(4);
    let mut char_buf = String::with_capacity(7);
    fn parse_num(n: &str, idx: usize) -> Result<TimeStrToken, ParserError> {
        Ok(TimeStrToken::Num(n.parse().map_err(|e| {
            ParserError::ParserError(format!(
                "Unable to parse value as a number at index {}: {}",
                idx, e
            ))
        })?))
    };
    fn maybe_tokenize_num_buf(
        n: &mut String,
        i: usize,
        t: &mut Vec<TimeStrToken>,
    ) -> Result<(), ParserError> {
        if !n.is_empty() {
            t.push(parse_num(&n, i)?);
            n.clear();
        }
        Ok(())
    }
    fn maybe_tokenize_char_buf(
        c: &mut String,
        t: &mut Vec<TimeStrToken>,
    ) -> Result<(), ParserError> {
        if !c.is_empty() {
            t.push(TimeStrToken::TimeUnit(DateTimeField::from_str(
                &c.to_uppercase(),
            )?));
            c.clear();
        }
        Ok(())
    }
    let mut last_field_is_frac = false;
    for (i, chr) in value.chars().enumerate() {
        if !num_buf.is_empty() && !char_buf.is_empty() {
            return Err(ParserError::TokenizerError(format!(
                "Invalid string in time-like type '{}': could not tokenize",
                value
            )));
        }
        match chr {
            '+' => {
                maybe_tokenize_num_buf(&mut num_buf, i, &mut toks)?;
                maybe_tokenize_char_buf(&mut char_buf, &mut toks)?;
                toks.push(TimeStrToken::Plus);
            }
            '-' => {
                maybe_tokenize_num_buf(&mut num_buf, i, &mut toks)?;
                maybe_tokenize_char_buf(&mut char_buf, &mut toks)?;
                toks.push(TimeStrToken::Dash);
            }
            ' ' => {
                maybe_tokenize_num_buf(&mut num_buf, i, &mut toks)?;
                maybe_tokenize_char_buf(&mut char_buf, &mut toks)?;
                toks.push(TimeStrToken::Space);
            }
            ':' => {
                maybe_tokenize_num_buf(&mut num_buf, i, &mut toks)?;
                maybe_tokenize_char_buf(&mut char_buf, &mut toks)?;
                toks.push(TimeStrToken::Colon);
            }
            '.' => {
                maybe_tokenize_num_buf(&mut num_buf, i, &mut toks)?;
                maybe_tokenize_char_buf(&mut char_buf, &mut toks)?;
                toks.push(TimeStrToken::Dot);
                last_field_is_frac = true;
            }
            chr if chr.is_digit(10) => {
                maybe_tokenize_char_buf(&mut char_buf, &mut toks)?;
                num_buf.push(chr)
            }
            chr if chr.is_ascii_alphabetic() => {
                maybe_tokenize_num_buf(&mut num_buf, i, &mut toks)?;
                char_buf.push(chr)
            }
            chr => {
                return Err(ParserError::TokenizerError(format!(
                    "Invalid character at offset {} in {}: {:?}",
                    i, value, chr
                )))
            }
        }
    }
    if !num_buf.is_empty() {
        if !last_field_is_frac {
            toks.push(parse_num(&num_buf, 0)?);
        } else {
            // this is guaranteed to be ascii, so len is fine
            let mut chars = num_buf.len();
            // Fractions only support 9 places of precision.
            let default_precision = 9;
            if chars > default_precision {
                num_buf = num_buf[..default_precision].to_string();
                chars = default_precision;
            }
            let raw: i64 = num_buf.parse().map_err(|e| {
                ParserError::ParserError(format!("couldn't parse fraction {}: {}", num_buf, e))
            })?;
            let multiplicand = 1_000_000_000 / 10_i64.pow(chars as u32);

            toks.push(TimeStrToken::Nanos(raw * multiplicand));
        }
    } else {
        maybe_tokenize_char_buf(&mut char_buf, &mut toks)?
    }
    Ok(toks)
}

fn tokenize_timezone(value: &str) -> Result<Vec<TimeStrToken>, ParserError> {
    let mut toks: Vec<TimeStrToken> = vec![];
    let mut num_buf = String::with_capacity(4);
    // If the timezone string has a colon, we need to parse all numbers naively.
    // Otherwise we need to parse long sequences of digits as [..hhhhmm]
    let split_nums: bool = !value.contains(':');

    // Takes a string and tries to parse it as a number token and insert it into
    // the token list
    fn parse_num(
        toks: &mut Vec<TimeStrToken>,
        n: &str,
        split_nums: bool,
        idx: usize,
    ) -> Result<(), ParserError> {
        if n.is_empty() {
            return Ok(());
        }

        let (first, second) = if n.len() > 2 && split_nums {
            let (first, second) = n.split_at(n.len() - 2);
            (first, Some(second))
        } else {
            (n, None)
        };

        toks.push(TimeStrToken::Num(first.parse().map_err(|e| {
                ParserError::ParserError(format!(
                    "Error tokenizing timezone string: unable to parse value {} as a number at index {}: {}",
                    first, idx, e
                ))
            })?));

        if let Some(second) = second {
            toks.push(TimeStrToken::Num(second.parse().map_err(|e| {
                ParserError::ParserError(format!(
                    "Error tokenizing timezone string: unable to parse value {} as a number at index {}: {}",
                    second, idx, e
                ))
            })?));
        }

        Ok(())
    };
    for (i, chr) in value.chars().enumerate() {
        match chr {
            '-' => {
                parse_num(&mut toks, &num_buf, split_nums, i)?;
                num_buf.clear();
                toks.push(TimeStrToken::Dash);
            }
            ' ' => {
                parse_num(&mut toks, &num_buf, split_nums, i)?;
                num_buf.clear();
                toks.push(TimeStrToken::Space);
            }
            ':' => {
                parse_num(&mut toks, &num_buf, split_nums, i)?;
                num_buf.clear();
                toks.push(TimeStrToken::Colon);
            }
            '+' => {
                parse_num(&mut toks, &num_buf, split_nums, i)?;
                num_buf.clear();
                toks.push(TimeStrToken::Plus);
            }
            chr if (chr == 'z' || chr == 'Z') && (i == value.len() - 1) => {
                parse_num(&mut toks, &num_buf, split_nums, i)?;
                num_buf.clear();
                toks.push(TimeStrToken::Zulu);
            }
            chr if chr.is_digit(10) => num_buf.push(chr),
            chr if chr.is_ascii_alphabetic() => {
                parse_num(&mut toks, &num_buf, split_nums, i)?;
                let substring = &value[i..];
                toks.push(TimeStrToken::TzName(substring.to_string()));
                return Ok(toks);
            }
            chr => {
                return Err(ParserError::TokenizerError(format!(
                    "Error tokenizing timezone string ({}): invalid character {:?} at offset {}",
                    value, chr, i
                )))
            }
        }
    }
    parse_num(&mut toks, &num_buf, split_nums, 0)?;
    Ok(toks)
}

fn build_timezone_offset_second(tokens: &[TimeStrToken], value: &str) -> Result<i64, ParserError> {
    use TimeStrToken::*;
    let all_formats = [
        vec![Plus, Num(0), Colon, Num(0)],
        vec![Dash, Num(0), Colon, Num(0)],
        vec![Plus, Num(0), Num(0)],
        vec![Dash, Num(0), Num(0)],
        vec![Plus, Num(0)],
        vec![Dash, Num(0)],
        vec![TzName("".to_string())],
        vec![Zulu],
    ];

    let mut is_positive = true;
    let mut hour_offset: Option<i64> = None;
    let mut minute_offset: Option<i64> = None;

    for format in all_formats.iter() {
        let actual = tokens.iter();

        if actual.len() != format.len() {
            continue;
        }

        for (i, (atok, etok)) in actual.zip(format).enumerate() {
            match (atok, etok) {
                (Colon, Colon) | (Plus, Plus) => { /* Matching punctuation */ }
                (Dash, Dash) => {
                    is_positive = false;
                }
                (Num(val), Num(_)) => {
                    let val = *val;
                    match (hour_offset, minute_offset) {
                        (None, None) => if val <= 24 {
                            hour_offset = Some(val as i64);
                        } else {
                            // We can return an error here because in all the
                            // formats with numbers we require the first number
                            // to be an hour and we require it to be <= 24
                            return Err(ParserError::ParserError(format!(
                                "Error parsing timezone string ({}): timezone hour invalid {}",
                                value, val
                            )));
                        }
                        (Some(_), None) => if val <= 60 {
                            minute_offset = Some(val as i64);
                        } else {
                            return Err(ParserError::ParserError(format!(
                                "Error parsing timezone string ({}): timezone minute invalid {}",
                                value, val
                            )));
                        },
                        // We've already seen an hour and a minute so we should
                        // never see another number
                        (Some(_), Some(_)) => return Err(ParserError::ParserError(format!(
                            "Error parsing timezone string ({}): invalid value {} at token index {}", value,
                            val, i
                        ))),
                        (None, Some(_)) => unreachable!("parsed a minute before an hour!"),
                    }
                }
                (Zulu, Zulu) => return Ok(0 as i64),
                (TzName(val), TzName(_)) => {
                    // For now, we don't support named timezones
                    return Err(ParserError::ParserError(format!(
                        "Error parsing timezone string ({}): named timezones are not supported. \
                         Failed to parse {} at token index {}",
                        value, val, i
                    )));
                }
                (_, _) => {
                    // Theres a mismatch between this format and the actual
                    // token stream Stop trying to parse in this format and go
                    // to the next one
                    is_positive = true;
                    hour_offset = None;
                    minute_offset = None;
                    break;
                }
            }
        }

        // Return the first valid parsed result
        if let Some(hour_offset) = hour_offset {
            let mut tz_offset_second: i64 = hour_offset * 60 * 60;

            if let Some(minute_offset) = minute_offset {
                tz_offset_second += minute_offset * 60;
            }

            if !is_positive {
                tz_offset_second *= -1
            }
            return Ok(tz_offset_second);
        }
    }

    Err(ParserError::ParserError("It didnt work".into()))
}

// fill_pdt_from_tokens populates the fields of a ParsedDateTime using
// the actual TimeStrTokens you received from the user and a set of expected
// TimeStrTokens.
fn fill_pdt_from_tokens(
    pdt: &mut ParsedDateTime,
    actual: &mut std::iter::Peekable<std::slice::Iter<'_, TimeStrToken>>,
    expected: &mut std::iter::Peekable<std::slice::Iter<'_, TimeStrToken>>,
    leading_field: DateTimeField,
    sign: i64,
) -> Result<(), failure::Error> {
    use TimeStrToken::*;
    let mut current_field = leading_field;

    let mut i = 0u8;

    let mut num_buf = 0_i64;
    let mut frac_buf = 0_i64;

    // Prevents PostgreSQL shorthand-style intervals (`'1h'`) from having the 'h' token parse
    // if not preceded by a number or nano position, while still allowing those fields to be
    // skipped over.
    let mut seen_num_or_nano = false;

    while let Some(atok) = actual.peek() {
        if let Some(etok) = expected.next() {
            match (atok, etok) {
                // The following forms of puncutation signal the end of a field and can
                // trigger a write.
                (Dash, Dash) | (Colon, Colon) => {
                    pdt.write_field_iff_none(
                        current_field,
                        Some(num_buf * sign),
                        Some(frac_buf * sign),
                    )?;
                    num_buf = 0;
                    frac_buf = 0;
                    current_field = current_field.next_smallest();
                    actual.next();
                }
                (Space, Space) => {
                    pdt.write_field_iff_none(
                        current_field,
                        Some(num_buf * sign),
                        Some(frac_buf * sign),
                    )?;
                    num_buf = 0;
                    frac_buf = 0;
                    current_field = current_field.next_smallest();
                    actual.next();
                    // PostgreSQL inexplicably trims all leading colons from all timestamp parts.
                    while let Some(Colon) = actual.peek() {
                        actual.next();
                    }
                }
                // Dots do not denote terminating a field, so should not trigger a write.
                (Dot, Dot) => {
                    actual.next();
                }
                (Num(val), Num(_)) => {
                    seen_num_or_nano = true;
                    num_buf = *val;
                    actual.next();
                }
                (Nanos(val), Nanos(_)) => {
                    seen_num_or_nano = true;
                    frac_buf = *val;
                    actual.next();
                }
                (Num(n), Nanos(_)) => {
                    seen_num_or_nano = true;
                    // Create disposable copy of n.
                    let mut nc = *n;

                    let mut width = 0;
                    // Destructively count the number of digits in n.
                    while nc != 0 {
                        nc /= 10;
                        width += 1;
                    }

                    let mut n = *n;

                    // Nanoseconds have 9 digits of precision.
                    let precision = 9;

                    if width > precision {
                        // Trim n to its 9 most significant digits.
                        n /= 10_i64.pow(width - precision);
                    } else {
                        // Right-pad n with 0s.
                        n *= 10_i64.pow(precision - width);
                    }

                    frac_buf = n;
                    actual.next();
                }
                (TimeUnit(f), TimeUnit(_)) if seen_num_or_nano => {
                    if *f != current_field {
                        failure::bail!(
                            "Invalid syntax at offset {}: provided {:?} but expected {:?}'",
                            i,
                            current_field,
                            f
                        )
                    }
                    actual.next();
                }
                (TimeUnit(f), TimeUnit(_)) if !seen_num_or_nano => failure::bail!(
                    "Invalid syntax at offset {}: {:?} must be preceeded by a number, e.g. '1{:?}'",
                    i,
                    f,
                    f
                ),
                // Allow skipping expected numbers, dots, and nanoseconds.
                (_, Num(_)) | (_, Dot) | (_, Nanos(_)) => {}
                (provided, expected) => failure::bail!(
                    "Invalid syntax at offset {}: provided {:?} but expected {:?}",
                    i,
                    provided,
                    expected
                ),
            }
        } else {
            // actual has more tokens than expected.
            failure::bail!(
                "Invalid syntax at offset {}: provided {:?} but expected None",
                i,
                atok,
            )
        };

        i += 1;
    }

    pdt.write_field_iff_none(
        current_field,
        Some(num_buf * sign),
        Some(frac_buf * sign as i64),
    )?;

    Ok(())
}

pub(crate) fn build_parsed_datetime_timestamp(value: &str) -> Result<ParsedDateTime, ParserError> {
    use TimeStrToken::*;
    let mut pdt = ParsedDateTime::default();

    let actual = tokenize_time_str(value)?;
    let mut actual = actual.iter().peekable();
    // PostgreSQL inexplicably trims all leading colons from all timestamp parts.
    while let Some(Colon) = actual.peek() {
        actual.next();
    }

    let expected = [
        Num(0), // year
        Dash,
        Num(0), // month
        Dash,
        Num(0), // day
        Space,
        Num(0), // hour
        Colon,
        Num(0), // minute
        Colon,
        Num(0), // second
        Dot,
        Nanos(0), // Nanos
    ];
    let mut expected = expected.iter().peekable();

    if let Err(e) =
        fill_pdt_from_tokens(&mut pdt, &mut actual, &mut expected, DateTimeField::Year, 1)
    {
        return parser_err!("Invalid DATE/TIME '{}'; {}", value, e);
    }

    Ok(pdt)
}

// Interval strings can be presented in one of two formats:
// - SQL Standard, e.g. `1-2 3 4:5:6.7`
// - PostgreSQL, e.g. `1 year 2 months 3 days`
// IntervalPartFormat indicates which type of parsing to use and encodes a
// DateTimeField, which indicates "where" you should begin parsing the
// associated tokens w/r/t their respective syntax.
enum IntervalPartFormat {
    SQLStandard(DateTimeField),
    PostgreSQL(DateTimeField),
}

// AnnotatedIntervalPart contains the tokens to be parsed, as well as the format
// to parse them.
struct AnnotatedIntervalPart {
    pub tokens: std::vec::Vec<TimeStrToken>,
    pub fmt: IntervalPartFormat,
}

// build_parsed_datetime converts the string portion of an interval (`value`)
// into a ParsedDateTime. You can allow the last part to be of an ambiguous
// format by including an `ambiguous_resolver` value.
pub(crate) fn build_parsed_datetime_interval(
    value: &str,
    ambiguous_resolver: DateTimeField,
) -> Result<ParsedDateTime, ParserError> {
    use DateTimeField::*;

    let mut pdt = ParsedDateTime::default();

    let mut value_parts = Vec::new();

    let value_split = value.trim().split_whitespace().collect::<Vec<&str>>();
    for s in value_split {
        value_parts.push(tokenize_time_str(s)?);
    }

    let mut value_parts = value_parts.iter().peekable();

    let mut annotated_parts = Vec::new();

    while let Some(part) = value_parts.next() {
        let mut fmt = determine_format_w_datetimefield(&part, value)?;
        // If you cannot determine the format of this part, try to infer its
        // format.
        if fmt.is_none() {
            fmt = match value_parts.next() {
                Some(next_part) => {
                    match determine_format_w_datetimefield(&next_part, value)? {
                        Some(IntervalPartFormat::SQLStandard(f)) => {
                            match f {
                                // Do not capture this token because expression
                                // is going to fail.
                                Year | Month | Day => None,
                                // If following part is H:M:S, infer that this
                                // part is Day. Because this part can use a
                                // fraction, it should be parsed as PostgreSQL.
                                _ => {
                                    // We can capture these annotated tokens
                                    // because expressions are commutative.
                                    annotated_parts.push(AnnotatedIntervalPart {
                                        fmt: IntervalPartFormat::SQLStandard(f),
                                        tokens: next_part.clone(),
                                    });
                                    Some(IntervalPartFormat::PostgreSQL(Day))
                                }
                            }
                        }
                        // None | Some(IntervalPartFormat::PostgreSQL(f))
                        // If next_fmt is IntervalPartFormat::PostgreSQL, that
                        // indicates that the following string was a TimeUnit,
                        // e.g. `day`, and this is where those tokens get
                        // consumed and propagated to their preceding numerical
                        // value.
                        next_fmt => next_fmt,
                    }
                }
                // Allow resolution of final part using ambiguous_resolver.
                None => Some(IntervalPartFormat::PostgreSQL(ambiguous_resolver)),
            }
        }
        match fmt {
            Some(fmt) => annotated_parts.push(AnnotatedIntervalPart {
                fmt,
                tokens: part.clone(),
            }),
            None => {
                return parser_err!(
                    "Invalid: INTERVAL '{}'; cannot determine format of all parts. Add \
                     explicit time components, e.g. INTERVAL '1 day' or INTERVAL '1' DAY.",
                    value
                )
            }
        }
    }

    for ap in annotated_parts {
        match ap.fmt {
            IntervalPartFormat::SQLStandard(f) => {
                build_parsed_datetime_sql_standard(&ap.tokens, f, value, &mut pdt)?
            }
            IntervalPartFormat::PostgreSQL(f) => {
                build_parsed_datetime_pg(&ap.tokens, f, value, &mut pdt)?
            }
        }
    }

    Ok(pdt)
}

// determine_format_w_datetimefield determines the format of the interval part
// (uses None to identify an indeterminant/ambiguous format) IntervalPartFormat
// also encodes the greatest DateTimeField in the token. This is necessary
// because the interval string format is not LL(1); we instead parse as few
// tokens as possible to generate the string's semantics.
fn determine_format_w_datetimefield(
    toks: &[TimeStrToken],
    interval_str: &str,
) -> Result<Option<IntervalPartFormat>, ParserError> {
    use DateTimeField::*;
    use IntervalPartFormat::*;
    use TimeStrToken::*;

    let mut toks = toks.iter().peekable();

    trim_interval_chars_return_sign(&mut toks);

    if let Some(Num(_)) = toks.peek() {
        toks.next();
    }

    match toks.next() {
        // Implies {?}{?}{?}, ambiguous case.
        None => Ok(None),
        Some(Dot) => {
            if let Some(Num(_)) = toks.peek() {
                toks.next();
            }
            match toks.peek() {
                // Implies {Num.NumTimeUnit}
                Some(TimeUnit(f)) => Ok(Some(PostgreSQL(*f))),
                // Implies {?}{?}{?}, ambiguous case.
                _ => Ok(None),
            }
        }
        // Implies {Y-...}{}{}
        Some(Dash) => Ok(Some(SQLStandard(Year))),
        // Implies {}{}{?:...}
        Some(Colon) => {
            if let Some(Num(_)) = toks.peek() {
                toks.next();
            }
            match toks.peek() {
                // Implies {H:M:...}
                Some(Colon) | None => Ok(Some(SQLStandard(Hour))),
                // Implies {M:S.NS}
                Some(Dot) => Ok(Some(SQLStandard(Minute))),
                _ => {
                    return parser_err!(
                        "Invalid: INTERVAL '{}': '{}' is not a well formed interval string",
                        interval_str,
                        interval_str
                    )
                }
            }
        }
        // Implies {Num}?{TimeUnit}
        Some(TimeUnit(f)) => Ok(Some(PostgreSQL(*f))),
        _ => {
            return parser_err!(
                "Invalid: INTERVAL '{}': '{}' is not a well formed interval string",
                interval_str,
                interval_str
            )
        }
    }
}

// build_parsed_datetime_sql_standard fills a ParsedDateTime's fields when
// encountering SQL standard-style interval parts, e.g. `1-2` for Y-M `4:5:6.7`
// for H:M:S.NS.
// Note that:
// - SQL-standard style groups ({Y-M}{D}{H:M:S.NS}) require that no fields in
//   the group have been modified, and do not allow any fields to be modified
//   afterward.
// - Single digits, e.g. `3` in `3 4:5:6.7` could be parsed as SQL standard
//   tokens, but end up being parsed as PostgreSQL-style tokens because of their
//   greater expressivity, in that they allow fractions, and otherwise-equivalence.
fn build_parsed_datetime_sql_standard(
    v: &[TimeStrToken],
    leading_field: DateTimeField,
    value: &str,
    mut pdt: &mut ParsedDateTime,
) -> Result<(), ParserError> {
    use DateTimeField::*;

    // Ensure that no fields have been previously modified.
    match leading_field {
        Year | Month => {
            if pdt.year.is_some() || pdt.month.is_some() {
                return parser_err!(
                    "Invalid INTERVAL '{}': YEAR or MONTH field set twice.",
                    value
                );
            }
        }
        Day => {
            if pdt.day.is_some() {
                return parser_err!("Invalid INTERVAL '{}': DAY field set twice.", value);
            }
        }
        // Hour Minute Second
        _ => {
            if pdt.hour.is_some()
                || pdt.minute.is_some()
                || pdt.second.is_some()
                || pdt.nano.is_some()
            {
                return parser_err!(
                    "Invalid INTERVAL '{}': HOUR, MINUTE, SECOND, or NANOSECOND field set twice.",
                    value
                );
            }
        }
    }

    let mut actual = v.iter().peekable();
    let expected = potential_sql_standard_interval_tokens(leading_field);
    let mut expected = expected.iter().peekable();

    let sign = trim_interval_chars_return_sign(&mut actual);

    if let Err(e) = fill_pdt_from_tokens(&mut pdt, &mut actual, &mut expected, leading_field, sign)
    {
        return parser_err!("Invalid: INTERVAL '{}'; {}", value, e);
    }

    // Do not allow any fields in the group to be modified afterward, and check
    // that values are valid. SQL standard-style interval parts do not allow
    // non-leading group components to "overflow" into the next-greatest
    // component, e.g. months cannot overflow into years.
    match leading_field {
        Year | Month => {
            if pdt.year.is_none() {
                pdt.year = Some(0);
            }
            match pdt.month {
                None => pdt.month = Some(0),
                Some(m) => {
                    if m >= 12 {
                        return parser_err!(
                            "Invalid INTERVAL '{}': MONTH field out range; \
                             must be < 12, have {}",
                            value,
                            m
                        );
                    }
                }
            }
        }
        Day => {
            if pdt.day.is_none() {
                pdt.day = Some(0);
            }
        }
        Hour | Minute | Second => {
            if pdt.hour.is_none() {
                pdt.hour = Some(0);
            }

            match pdt.minute {
                None => pdt.minute = Some(0),
                Some(m) => {
                    if m >= 60 {
                        return parser_err!(
                            "Invalid INTERVAL '{}': MINUTE field out range; \
                             must be < 60, have {}",
                            value,
                            m
                        );
                    }
                }
            }

            match pdt.second {
                None => pdt.second = Some(0),
                Some(s) => {
                    if s >= 60 {
                        return parser_err!(
                            "Invalid INTERVAL '{}': SECOND field out range; \
                             must be < 60, have {}",
                            value,
                            s
                        );
                    }
                }
            }

            if pdt.nano.is_none() {
                pdt.nano = Some(0);
            }
        }
    }

    Ok(())
}

/// Get the tokens that you *might* end up parsing starting with a most
/// significant unit, and ending with a least significant unit.
/// Space tokens are never actually included in the output, but are
/// illustrative of what the expected input of SQL Standard interval
/// values looks like.
fn potential_sql_standard_interval_tokens(from: DateTimeField) -> Vec<TimeStrToken> {
    use DateTimeField::*;
    use TimeStrToken::*;

    let all_toks = [
        Num(0), // year
        Dash,
        Num(0), // month
        Space,
        Num(0), // day
        Space,
        Num(0), // hour
        Colon,
        Num(0), // minute
        Colon,
        Num(0), // second
        Dot,
        Nanos(0), // Nanos
    ];
    let (start, end) = match from {
        Year => (0, 4),
        Month => (2, 4),
        Day => (4, 6),
        Hour => (6, 13),
        Minute => (8, 13),
        Second => (10, 13),
    };
    all_toks[start..end].to_vec()
}

// build_parsed_datetime_pg fills a ParsedDateTime's fields when encountering
// PostgreSQL-style interval parts, e.g. `1 month 2 days`.
// Note that:
// - This function only meaningfully parses the numerical component of the
//   string, and relies on determining the DateTimeField component from
//   AnnotatedIntervalPart, passed in as `time_unit`.
// - Only PostgreSQL-style parts can use fractional components in positions
//   other than seconds, e.g. `1.5 months`.
fn build_parsed_datetime_pg(
    tokens: &[TimeStrToken],
    time_unit: DateTimeField,
    value: &str,
    mut pdt: &mut ParsedDateTime,
) -> Result<(), ParserError> {
    use TimeStrToken::*;

    let mut actual = tokens.iter().peekable();
    // We remove all spaces during tokenization, so TimeUnit only shows up if
    // there is no space between the number and the TimeUnit, e.g. `1y 2d 3h`, which
    // PostgreSQL allows.
    let expected = vec![Num(0), Dot, Nanos(0), TimeUnit(DateTimeField::Year)];
    let mut expected = expected.iter().peekable();

    let sign = trim_interval_chars_return_sign(&mut actual);

    if let Err(e) = fill_pdt_from_tokens(&mut pdt, &mut actual, &mut expected, time_unit, sign) {
        return parser_err!("Invalid INTERVAL '{}': {}", value, e);
    }

    Ok(())
}

// Trims tokens equivalent to regex (:*(+|-)?) and returns a value reflecting
// the expressed sign: 1 for positive, -1 for negative.
fn trim_interval_chars_return_sign(
    z: &mut std::iter::Peekable<std::slice::Iter<'_, TimeStrToken>>,
) -> i64 {
    use TimeStrToken::*;

    // PostgreSQL inexplicably trims all leading colons from interval parts.
    while let Some(Colon) = z.peek() {
        z.next();
    }

    match z.peek() {
        Some(Dash) => {
            z.next();
            -1
        }
        Some(Plus) => {
            z.next();
            1
        }
        _ => 1,
    }
}

/// Takes a 'date timezone' 'date time timezone' string and splits it into 'date
/// {time}' and 'timezone' components
pub(crate) fn split_timestamp_string(value: &str) -> (&str, &str) {
    // First we need to see if the string contains " +" or " -" because
    // timestamps can come in a format YYYY-MM-DD {+|-}<tz> (where the timezone
    // string can have colons)
    let cut = value.find(" +").or_else(|| value.find(" -"));

    if let Some(cut) = cut {
        let (first, second) = value.split_at(cut);
        return (first.trim(), second.trim());
    }

    // If we have a hh:mm:dd component, we need to go past that to see if we can
    // find a tz
    let colon = value.find(':');

    if let Some(colon) = colon {
        let substring = value.get(colon..);
        if let Some(substring) = substring {
            let tz = substring
                .find(|c: char| (c == '-') || (c == '+') || (c == ' ') || c.is_ascii_alphabetic());

            if let Some(tz) = tz {
                let (first, second) = value.split_at(colon + tz);
                return (first.trim(), second.trim());
            }
        }

        (value.trim(), "")
    } else {
        // We don't have a time, so the only formats available are YYY-mm-dd<tz>
        // or YYYY-MM-dd <tz> Numeric offset timezones need to be separated from
        // the ymd by a space
        let cut = value.find(|c: char| (c == ' ') || c.is_ascii_alphabetic());

        if let Some(cut) = cut {
            let (first, second) = value.split_at(cut);
            return (first.trim(), second.trim());
        }

        (value.trim(), "")
    }
}

pub(crate) fn parse_timezone_offset_second(value: &str) -> Result<i64, ParserError> {
    let toks = tokenize_timezone(value)?;
    Ok(build_timezone_offset_second(&toks, value)?)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::parser::*;

    #[test]
    fn test_potential_interval_tokens() {
        use DateTimeField::*;
        use TimeStrToken::*;
        assert_eq!(
            potential_sql_standard_interval_tokens(Year),
            vec![Num(0), Dash, Num(0), Space]
        );

        assert_eq!(
            potential_sql_standard_interval_tokens(Day),
            vec![Num(0), Space,]
        );
        assert_eq!(
            potential_sql_standard_interval_tokens(Hour),
            vec![Num(0), Colon, Num(0), Colon, Num(0), Dot, Nanos(0)]
        );
    }
    #[test]
    fn test_trim_interval_chars_return_sign() {
        let s = tokenize_time_str("::::-2").unwrap();
        let mut s = s.iter().peekable();

        assert_eq!(trim_interval_chars_return_sign(&mut s), -1);
        assert_eq!(**s.peek().unwrap(), tokenize_time_str("2").unwrap()[0]);

        let s = tokenize_time_str("-3").unwrap();
        let mut s = s.iter().peekable();

        assert_eq!(trim_interval_chars_return_sign(&mut s), -1);
        assert_eq!(**s.peek().unwrap(), tokenize_time_str("3").unwrap()[0]);

        let s = tokenize_time_str("::::+4").unwrap();
        let mut s = s.iter().peekable();

        assert_eq!(trim_interval_chars_return_sign(&mut s), 1);
        assert_eq!(**s.peek().unwrap(), tokenize_time_str("4").unwrap()[0]);

        let s = tokenize_time_str("+5").unwrap();
        let mut s = s.iter().peekable();

        assert_eq!(trim_interval_chars_return_sign(&mut s), 1);
        assert_eq!(**s.peek().unwrap(), tokenize_time_str("5").unwrap()[0]);

        let s = tokenize_time_str("-::::").unwrap();
        let mut s = s.iter().peekable();

        assert_eq!(trim_interval_chars_return_sign(&mut s), -1);
        assert_eq!(**s.peek().unwrap(), tokenize_time_str(":").unwrap()[0]);

        let s = tokenize_time_str("-YEAR").unwrap();
        let mut s = s.iter().peekable();

        assert_eq!(trim_interval_chars_return_sign(&mut s), -1);
        assert_eq!(**s.peek().unwrap(), tokenize_time_str("YEAR").unwrap()[0]);
    }

    #[test]
    fn test_split_timestamp_string() {
        let test_cases = [
            (
                "1969-06-01 10:10:10.410 UTC",
                "1969-06-01 10:10:10.410",
                "UTC",
            ),
            (
                "1969-06-01 10:10:10.410+4:00",
                "1969-06-01 10:10:10.410",
                "+4:00",
            ),
            (
                "1969-06-01 10:10:10.410-4:00",
                "1969-06-01 10:10:10.410",
                "-4:00",
            ),
            ("1969-06-01 10:10:10.410", "1969-06-01 10:10:10.410", ""),
            ("1969-06-01 10:10:10.410+4", "1969-06-01 10:10:10.410", "+4"),
            ("1969-06-01 10:10:10.410-4", "1969-06-01 10:10:10.410", "-4"),
            ("1969-06-01 10:10:10+4:00", "1969-06-01 10:10:10", "+4:00"),
            ("1969-06-01 10:10:10-4:00", "1969-06-01 10:10:10", "-4:00"),
            ("1969-06-01 10:10:10 UTC", "1969-06-01 10:10:10", "UTC"),
            ("1969-06-01 10:10:10", "1969-06-01 10:10:10", ""),
            ("1969-06-01 10:10+4:00", "1969-06-01 10:10", "+4:00"),
            ("1969-06-01 10:10-4:00", "1969-06-01 10:10", "-4:00"),
            ("1969-06-01 10:10 UTC", "1969-06-01 10:10", "UTC"),
            ("1969-06-01 10:10", "1969-06-01 10:10", ""),
            ("1969-06-01 UTC", "1969-06-01", "UTC"),
            ("1969-06-01 +4:00", "1969-06-01", "+4:00"),
            ("1969-06-01 -4:00", "1969-06-01", "-4:00"),
            ("1969-06-01 +4", "1969-06-01", "+4"),
            ("1969-06-01 -4", "1969-06-01", "-4"),
            ("1969-06-01", "1969-06-01", ""),
            ("1969-06-01 10:10:10.410Z", "1969-06-01 10:10:10.410", "Z"),
            ("1969-06-01 10:10:10.410z", "1969-06-01 10:10:10.410", "z"),
            ("1969-06-01Z", "1969-06-01", "Z"),
            ("1969-06-01z", "1969-06-01", "z"),
            ("1969-06-01 10:10:10.410   ", "1969-06-01 10:10:10.410", ""),
            (
                "1969-06-01     10:10:10.410   ",
                "1969-06-01     10:10:10.410",
                "",
            ),
            ("   1969-06-01 10:10:10.412", "1969-06-01 10:10:10.412", ""),
            (
                "   1969-06-01 10:10:10.413   ",
                "1969-06-01 10:10:10.413",
                "",
            ),
            (
                "1969-06-01 10:10:10.410 +4:00",
                "1969-06-01 10:10:10.410",
                "+4:00",
            ),
            (
                "1969-06-01 10:10:10.410+4 :00",
                "1969-06-01 10:10:10.410",
                "+4 :00",
            ),
            (
                "1969-06-01 10:10:10.410      +4:00",
                "1969-06-01 10:10:10.410",
                "+4:00",
            ),
            (
                "1969-06-01 10:10:10.410+4:00     ",
                "1969-06-01 10:10:10.410",
                "+4:00",
            ),
            (
                "1969-06-01 10:10:10.410  Z  ",
                "1969-06-01 10:10:10.410",
                "Z",
            ),
            ("1969-06-01    +4  ", "1969-06-01", "+4"),
            ("1969-06-01   Z   ", "1969-06-01", "Z"),
        ];

        for test in test_cases.iter() {
            let (ts, tz) = split_timestamp_string(test.0);

            assert_eq!(ts, test.1);
            assert_eq!(tz, test.2);
        }
    }

    #[test]
    fn test_parse_timezone_offset_second() {
        let test_cases = [
            ("+0:00", 0),
            ("-0:00", 0),
            ("+0:000000", 0),
            ("+000000:00", 0),
            ("+000000:000000", 0),
            ("+0", 0),
            ("+00", 0),
            ("+000", 0),
            ("+0000", 0),
            ("+00000000", 0),
            ("+0000001:000000", 3600),
            ("+0000000:000001", 60),
            ("+0000001:000001", 3660),
            ("+4:00", 14400),
            ("-4:00", -14400),
            ("+2:30", 9000),
            ("-5:15", -18900),
            ("+0:20", 1200),
            ("-0:20", -1200),
            ("+5", 18000),
            ("-5", -18000),
            ("+05", 18000),
            ("-05", -18000),
            ("+500", 18000),
            ("-500", -18000),
            ("+530", 19800),
            ("-530", -19800),
            ("+050", 3000),
            ("-050", -3000),
            ("+15", 54000),
            ("-15", -54000),
            ("+1515", 54900),
            ("+015", 900),
            ("-015", -900),
            ("+0015", 900),
            ("-0015", -900),
            ("+00015", 900),
            ("-00015", -900),
            ("+005", 300),
            ("-005", -300),
            ("+0000005", 300),
            ("+00000100", 3600),
            ("Z", 0),
            ("z", 0),
        ];

        for test in test_cases.iter() {
            match parse_timezone_offset_second(test.0) {
                Ok(tz_offset) => {
                    let expected: i64 = test.1 as i64;

                    println!("{} {}", expected, tz_offset);
                    assert_eq!(tz_offset, expected);
                }
                Err(e) => panic!(
                    "Test failed when expected to pass test case: {} error: {}",
                    test.0, e
                ),
            }
        }

        let failure_test_cases = [
            "+25:00", "+120:00", "+0:61", "+0:500", " 12:30", "+-12:30", "+2525", "+2561",
            "+255900", "+25", "+5::30", "+5:30:", "+5:30:16", "+5:", "++5:00", "--5:00", "UTC",
            " UTC", "a", "zzz", "ZZZ", "ZZ Top", " +", " -", " ", "1", "12", "1234",
        ];

        for test in failure_test_cases.iter() {
            match parse_timezone_offset_second(test) {
                Ok(t) => panic!("Test passed when expected to fail test case: {} parsed tz offset (seconds): {}", test, t),
                Err(e) => println!("{}", e),
            }
        }
    }
}
