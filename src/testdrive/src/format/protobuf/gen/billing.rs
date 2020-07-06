// This file is generated by rust-protobuf 2.16.2. Do not edit
// @generated

// https://github.com/rust-lang/rust-clippy/issues/702
#![allow(unknown_lints)]
#![allow(clippy::all)]

#![allow(unused_attributes)]
#![rustfmt::skip]

#![allow(box_pointers)]
#![allow(dead_code)]
#![allow(missing_docs)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(trivial_casts)]
#![allow(unused_imports)]
#![allow(unused_results)]
//! Generated file from `src/format/protobuf/billing.proto`

/// Generated files are compatible only with the same version
/// of protobuf runtime.
// const _PROTOBUF_VERSION_CHECK: () = ::protobuf::VERSION_2_16_2;

#[derive(PartialEq,Clone,Default)]
pub struct Measurement {
    // message fields
    pub resource: Resource,
    pub measured_value: i64,
    // special fields
    pub unknown_fields: ::protobuf::UnknownFields,
    pub cached_size: ::protobuf::CachedSize,
}

impl<'a> ::std::default::Default for &'a Measurement {
    fn default() -> &'a Measurement {
        <Measurement as ::protobuf::Message>::default_instance()
    }
}

impl Measurement {
    pub fn new() -> Measurement {
        ::std::default::Default::default()
    }

    // .Resource resource = 1;


    pub fn get_resource(&self) -> Resource {
        self.resource
    }
    pub fn clear_resource(&mut self) {
        self.resource = Resource::NULL;
    }

    // Param is passed by value, moved
    pub fn set_resource(&mut self, v: Resource) {
        self.resource = v;
    }

    // int64 measured_value = 2;


    pub fn get_measured_value(&self) -> i64 {
        self.measured_value
    }
    pub fn clear_measured_value(&mut self) {
        self.measured_value = 0;
    }

    // Param is passed by value, moved
    pub fn set_measured_value(&mut self, v: i64) {
        self.measured_value = v;
    }
}

impl ::protobuf::Message for Measurement {
    fn is_initialized(&self) -> bool {
        true
    }

    fn merge_from(&mut self, is: &mut ::protobuf::CodedInputStream<'_>) -> ::protobuf::ProtobufResult<()> {
        while !is.eof()? {
            let (field_number, wire_type) = is.read_tag_unpack()?;
            match field_number {
                1 => {
                    ::protobuf::rt::read_proto3_enum_with_unknown_fields_into(wire_type, is, &mut self.resource, 1, &mut self.unknown_fields)?
                },
                2 => {
                    if wire_type != ::protobuf::wire_format::WireTypeVarint {
                        return ::std::result::Result::Err(::protobuf::rt::unexpected_wire_type(wire_type));
                    }
                    let tmp = is.read_int64()?;
                    self.measured_value = tmp;
                },
                _ => {
                    ::protobuf::rt::read_unknown_or_skip_group(field_number, wire_type, is, self.mut_unknown_fields())?;
                },
            };
        }
        ::std::result::Result::Ok(())
    }

    // Compute sizes of nested messages
    #[allow(unused_variables)]
    fn compute_size(&self) -> u32 {
        let mut my_size = 0;
        if self.resource != Resource::NULL {
            my_size += ::protobuf::rt::enum_size(1, self.resource);
        }
        if self.measured_value != 0 {
            my_size += ::protobuf::rt::value_size(2, self.measured_value, ::protobuf::wire_format::WireTypeVarint);
        }
        my_size += ::protobuf::rt::unknown_fields_size(self.get_unknown_fields());
        self.cached_size.set(my_size);
        my_size
    }

    fn write_to_with_cached_sizes(&self, os: &mut ::protobuf::CodedOutputStream<'_>) -> ::protobuf::ProtobufResult<()> {
        if self.resource != Resource::NULL {
            os.write_enum(1, ::protobuf::ProtobufEnum::value(&self.resource))?;
        }
        if self.measured_value != 0 {
            os.write_int64(2, self.measured_value)?;
        }
        os.write_unknown_fields(self.get_unknown_fields())?;
        ::std::result::Result::Ok(())
    }

    fn get_cached_size(&self) -> u32 {
        self.cached_size.get()
    }

    fn get_unknown_fields(&self) -> &::protobuf::UnknownFields {
        &self.unknown_fields
    }

    fn mut_unknown_fields(&mut self) -> &mut ::protobuf::UnknownFields {
        &mut self.unknown_fields
    }

    fn as_any(&self) -> &dyn (::std::any::Any) {
        self as &dyn (::std::any::Any)
    }
    fn as_any_mut(&mut self) -> &mut dyn (::std::any::Any) {
        self as &mut dyn (::std::any::Any)
    }
    fn into_any(self: ::std::boxed::Box<Self>) -> ::std::boxed::Box<dyn (::std::any::Any)> {
        self
    }

    fn descriptor(&self) -> &'static ::protobuf::reflect::MessageDescriptor {
        Self::descriptor_static()
    }

    fn new() -> Measurement {
        Measurement::new()
    }

    fn descriptor_static() -> &'static ::protobuf::reflect::MessageDescriptor {
        static descriptor: ::protobuf::rt::LazyV2<::protobuf::reflect::MessageDescriptor> = ::protobuf::rt::LazyV2::INIT;
        descriptor.get(|| {
            let mut fields = ::std::vec::Vec::new();
            fields.push(::protobuf::reflect::accessor::make_simple_field_accessor::<_, ::protobuf::types::ProtobufTypeEnum<Resource>>(
                "resource",
                |m: &Measurement| { &m.resource },
                |m: &mut Measurement| { &mut m.resource },
            ));
            fields.push(::protobuf::reflect::accessor::make_simple_field_accessor::<_, ::protobuf::types::ProtobufTypeInt64>(
                "measured_value",
                |m: &Measurement| { &m.measured_value },
                |m: &mut Measurement| { &mut m.measured_value },
            ));
            ::protobuf::reflect::MessageDescriptor::new_pb_name::<Measurement>(
                "Measurement",
                fields,
                file_descriptor_proto()
            )
        })
    }

    fn default_instance() -> &'static Measurement {
        static instance: ::protobuf::rt::LazyV2<Measurement> = ::protobuf::rt::LazyV2::INIT;
        instance.get(Measurement::new)
    }
}

impl ::protobuf::Clear for Measurement {
    fn clear(&mut self) {
        self.resource = Resource::NULL;
        self.measured_value = 0;
        self.unknown_fields.clear();
    }
}

impl ::std::fmt::Debug for Measurement {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        ::protobuf::text_format::fmt(self, f)
    }
}

impl ::protobuf::reflect::ProtobufValue for Measurement {
    fn as_ref(&self) -> ::protobuf::reflect::ReflectValueRef {
        ::protobuf::reflect::ReflectValueRef::Message(self)
    }
}

#[derive(PartialEq,Clone,Default)]
pub struct Record {
    // message fields
    pub interval_start: ::std::string::String,
    pub interval_end: ::std::string::String,
    pub meter: ::std::string::String,
    pub value: u32,
    pub measurements: ::protobuf::RepeatedField<Measurement>,
    // special fields
    pub unknown_fields: ::protobuf::UnknownFields,
    pub cached_size: ::protobuf::CachedSize,
}

impl<'a> ::std::default::Default for &'a Record {
    fn default() -> &'a Record {
        <Record as ::protobuf::Message>::default_instance()
    }
}

impl Record {
    pub fn new() -> Record {
        ::std::default::Default::default()
    }

    // string interval_start = 1;


    pub fn get_interval_start(&self) -> &str {
        &self.interval_start
    }
    pub fn clear_interval_start(&mut self) {
        self.interval_start.clear();
    }

    // Param is passed by value, moved
    pub fn set_interval_start(&mut self, v: ::std::string::String) {
        self.interval_start = v;
    }

    // Mutable pointer to the field.
    // If field is not initialized, it is initialized with default value first.
    pub fn mut_interval_start(&mut self) -> &mut ::std::string::String {
        &mut self.interval_start
    }

    // Take field
    pub fn take_interval_start(&mut self) -> ::std::string::String {
        ::std::mem::replace(&mut self.interval_start, ::std::string::String::new())
    }

    // string interval_end = 2;


    pub fn get_interval_end(&self) -> &str {
        &self.interval_end
    }
    pub fn clear_interval_end(&mut self) {
        self.interval_end.clear();
    }

    // Param is passed by value, moved
    pub fn set_interval_end(&mut self, v: ::std::string::String) {
        self.interval_end = v;
    }

    // Mutable pointer to the field.
    // If field is not initialized, it is initialized with default value first.
    pub fn mut_interval_end(&mut self) -> &mut ::std::string::String {
        &mut self.interval_end
    }

    // Take field
    pub fn take_interval_end(&mut self) -> ::std::string::String {
        ::std::mem::replace(&mut self.interval_end, ::std::string::String::new())
    }

    // string meter = 3;


    pub fn get_meter(&self) -> &str {
        &self.meter
    }
    pub fn clear_meter(&mut self) {
        self.meter.clear();
    }

    // Param is passed by value, moved
    pub fn set_meter(&mut self, v: ::std::string::String) {
        self.meter = v;
    }

    // Mutable pointer to the field.
    // If field is not initialized, it is initialized with default value first.
    pub fn mut_meter(&mut self) -> &mut ::std::string::String {
        &mut self.meter
    }

    // Take field
    pub fn take_meter(&mut self) -> ::std::string::String {
        ::std::mem::replace(&mut self.meter, ::std::string::String::new())
    }

    // uint32 value = 4;


    pub fn get_value(&self) -> u32 {
        self.value
    }
    pub fn clear_value(&mut self) {
        self.value = 0;
    }

    // Param is passed by value, moved
    pub fn set_value(&mut self, v: u32) {
        self.value = v;
    }

    // repeated .Measurement measurements = 7;


    pub fn get_measurements(&self) -> &[Measurement] {
        &self.measurements
    }
    pub fn clear_measurements(&mut self) {
        self.measurements.clear();
    }

    // Param is passed by value, moved
    pub fn set_measurements(&mut self, v: ::protobuf::RepeatedField<Measurement>) {
        self.measurements = v;
    }

    // Mutable pointer to the field.
    pub fn mut_measurements(&mut self) -> &mut ::protobuf::RepeatedField<Measurement> {
        &mut self.measurements
    }

    // Take field
    pub fn take_measurements(&mut self) -> ::protobuf::RepeatedField<Measurement> {
        ::std::mem::replace(&mut self.measurements, ::protobuf::RepeatedField::new())
    }
}

impl ::protobuf::Message for Record {
    fn is_initialized(&self) -> bool {
        for v in &self.measurements {
            if !v.is_initialized() {
                return false;
            }
        };
        true
    }

    fn merge_from(&mut self, is: &mut ::protobuf::CodedInputStream<'_>) -> ::protobuf::ProtobufResult<()> {
        while !is.eof()? {
            let (field_number, wire_type) = is.read_tag_unpack()?;
            match field_number {
                1 => {
                    ::protobuf::rt::read_singular_proto3_string_into(wire_type, is, &mut self.interval_start)?;
                },
                2 => {
                    ::protobuf::rt::read_singular_proto3_string_into(wire_type, is, &mut self.interval_end)?;
                },
                3 => {
                    ::protobuf::rt::read_singular_proto3_string_into(wire_type, is, &mut self.meter)?;
                },
                4 => {
                    if wire_type != ::protobuf::wire_format::WireTypeVarint {
                        return ::std::result::Result::Err(::protobuf::rt::unexpected_wire_type(wire_type));
                    }
                    let tmp = is.read_uint32()?;
                    self.value = tmp;
                },
                7 => {
                    ::protobuf::rt::read_repeated_message_into(wire_type, is, &mut self.measurements)?;
                },
                _ => {
                    ::protobuf::rt::read_unknown_or_skip_group(field_number, wire_type, is, self.mut_unknown_fields())?;
                },
            };
        }
        ::std::result::Result::Ok(())
    }

    // Compute sizes of nested messages
    #[allow(unused_variables)]
    fn compute_size(&self) -> u32 {
        let mut my_size = 0;
        if !self.interval_start.is_empty() {
            my_size += ::protobuf::rt::string_size(1, &self.interval_start);
        }
        if !self.interval_end.is_empty() {
            my_size += ::protobuf::rt::string_size(2, &self.interval_end);
        }
        if !self.meter.is_empty() {
            my_size += ::protobuf::rt::string_size(3, &self.meter);
        }
        if self.value != 0 {
            my_size += ::protobuf::rt::value_size(4, self.value, ::protobuf::wire_format::WireTypeVarint);
        }
        for value in &self.measurements {
            let len = value.compute_size();
            my_size += 1 + ::protobuf::rt::compute_raw_varint32_size(len) + len;
        };
        my_size += ::protobuf::rt::unknown_fields_size(self.get_unknown_fields());
        self.cached_size.set(my_size);
        my_size
    }

    fn write_to_with_cached_sizes(&self, os: &mut ::protobuf::CodedOutputStream<'_>) -> ::protobuf::ProtobufResult<()> {
        if !self.interval_start.is_empty() {
            os.write_string(1, &self.interval_start)?;
        }
        if !self.interval_end.is_empty() {
            os.write_string(2, &self.interval_end)?;
        }
        if !self.meter.is_empty() {
            os.write_string(3, &self.meter)?;
        }
        if self.value != 0 {
            os.write_uint32(4, self.value)?;
        }
        for v in &self.measurements {
            os.write_tag(7, ::protobuf::wire_format::WireTypeLengthDelimited)?;
            os.write_raw_varint32(v.get_cached_size())?;
            v.write_to_with_cached_sizes(os)?;
        };
        os.write_unknown_fields(self.get_unknown_fields())?;
        ::std::result::Result::Ok(())
    }

    fn get_cached_size(&self) -> u32 {
        self.cached_size.get()
    }

    fn get_unknown_fields(&self) -> &::protobuf::UnknownFields {
        &self.unknown_fields
    }

    fn mut_unknown_fields(&mut self) -> &mut ::protobuf::UnknownFields {
        &mut self.unknown_fields
    }

    fn as_any(&self) -> &dyn (::std::any::Any) {
        self as &dyn (::std::any::Any)
    }
    fn as_any_mut(&mut self) -> &mut dyn (::std::any::Any) {
        self as &mut dyn (::std::any::Any)
    }
    fn into_any(self: ::std::boxed::Box<Self>) -> ::std::boxed::Box<dyn (::std::any::Any)> {
        self
    }

    fn descriptor(&self) -> &'static ::protobuf::reflect::MessageDescriptor {
        Self::descriptor_static()
    }

    fn new() -> Record {
        Record::new()
    }

    fn descriptor_static() -> &'static ::protobuf::reflect::MessageDescriptor {
        static descriptor: ::protobuf::rt::LazyV2<::protobuf::reflect::MessageDescriptor> = ::protobuf::rt::LazyV2::INIT;
        descriptor.get(|| {
            let mut fields = ::std::vec::Vec::new();
            fields.push(::protobuf::reflect::accessor::make_simple_field_accessor::<_, ::protobuf::types::ProtobufTypeString>(
                "interval_start",
                |m: &Record| { &m.interval_start },
                |m: &mut Record| { &mut m.interval_start },
            ));
            fields.push(::protobuf::reflect::accessor::make_simple_field_accessor::<_, ::protobuf::types::ProtobufTypeString>(
                "interval_end",
                |m: &Record| { &m.interval_end },
                |m: &mut Record| { &mut m.interval_end },
            ));
            fields.push(::protobuf::reflect::accessor::make_simple_field_accessor::<_, ::protobuf::types::ProtobufTypeString>(
                "meter",
                |m: &Record| { &m.meter },
                |m: &mut Record| { &mut m.meter },
            ));
            fields.push(::protobuf::reflect::accessor::make_simple_field_accessor::<_, ::protobuf::types::ProtobufTypeUint32>(
                "value",
                |m: &Record| { &m.value },
                |m: &mut Record| { &mut m.value },
            ));
            fields.push(::protobuf::reflect::accessor::make_repeated_field_accessor::<_, ::protobuf::types::ProtobufTypeMessage<Measurement>>(
                "measurements",
                |m: &Record| { &m.measurements },
                |m: &mut Record| { &mut m.measurements },
            ));
            ::protobuf::reflect::MessageDescriptor::new_pb_name::<Record>(
                "Record",
                fields,
                file_descriptor_proto()
            )
        })
    }

    fn default_instance() -> &'static Record {
        static instance: ::protobuf::rt::LazyV2<Record> = ::protobuf::rt::LazyV2::INIT;
        instance.get(Record::new)
    }
}

impl ::protobuf::Clear for Record {
    fn clear(&mut self) {
        self.interval_start.clear();
        self.interval_end.clear();
        self.meter.clear();
        self.value = 0;
        self.measurements.clear();
        self.unknown_fields.clear();
    }
}

impl ::std::fmt::Debug for Record {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        ::protobuf::text_format::fmt(self, f)
    }
}

impl ::protobuf::reflect::ProtobufValue for Record {
    fn as_ref(&self) -> ::protobuf::reflect::ReflectValueRef {
        ::protobuf::reflect::ReflectValueRef::Message(self)
    }
}

#[derive(PartialEq,Clone,Default)]
pub struct Batch {
    // message fields
    pub id: ::std::string::String,
    pub interval_start: ::std::string::String,
    pub interval_end: ::std::string::String,
    pub records: ::protobuf::RepeatedField<Record>,
    // special fields
    pub unknown_fields: ::protobuf::UnknownFields,
    pub cached_size: ::protobuf::CachedSize,
}

impl<'a> ::std::default::Default for &'a Batch {
    fn default() -> &'a Batch {
        <Batch as ::protobuf::Message>::default_instance()
    }
}

impl Batch {
    pub fn new() -> Batch {
        ::std::default::Default::default()
    }

    // string id = 1;


    pub fn get_id(&self) -> &str {
        &self.id
    }
    pub fn clear_id(&mut self) {
        self.id.clear();
    }

    // Param is passed by value, moved
    pub fn set_id(&mut self, v: ::std::string::String) {
        self.id = v;
    }

    // Mutable pointer to the field.
    // If field is not initialized, it is initialized with default value first.
    pub fn mut_id(&mut self) -> &mut ::std::string::String {
        &mut self.id
    }

    // Take field
    pub fn take_id(&mut self) -> ::std::string::String {
        ::std::mem::replace(&mut self.id, ::std::string::String::new())
    }

    // string interval_start = 3;


    pub fn get_interval_start(&self) -> &str {
        &self.interval_start
    }
    pub fn clear_interval_start(&mut self) {
        self.interval_start.clear();
    }

    // Param is passed by value, moved
    pub fn set_interval_start(&mut self, v: ::std::string::String) {
        self.interval_start = v;
    }

    // Mutable pointer to the field.
    // If field is not initialized, it is initialized with default value first.
    pub fn mut_interval_start(&mut self) -> &mut ::std::string::String {
        &mut self.interval_start
    }

    // Take field
    pub fn take_interval_start(&mut self) -> ::std::string::String {
        ::std::mem::replace(&mut self.interval_start, ::std::string::String::new())
    }

    // string interval_end = 4;


    pub fn get_interval_end(&self) -> &str {
        &self.interval_end
    }
    pub fn clear_interval_end(&mut self) {
        self.interval_end.clear();
    }

    // Param is passed by value, moved
    pub fn set_interval_end(&mut self, v: ::std::string::String) {
        self.interval_end = v;
    }

    // Mutable pointer to the field.
    // If field is not initialized, it is initialized with default value first.
    pub fn mut_interval_end(&mut self) -> &mut ::std::string::String {
        &mut self.interval_end
    }

    // Take field
    pub fn take_interval_end(&mut self) -> ::std::string::String {
        ::std::mem::replace(&mut self.interval_end, ::std::string::String::new())
    }

    // repeated .Record records = 7;


    pub fn get_records(&self) -> &[Record] {
        &self.records
    }
    pub fn clear_records(&mut self) {
        self.records.clear();
    }

    // Param is passed by value, moved
    pub fn set_records(&mut self, v: ::protobuf::RepeatedField<Record>) {
        self.records = v;
    }

    // Mutable pointer to the field.
    pub fn mut_records(&mut self) -> &mut ::protobuf::RepeatedField<Record> {
        &mut self.records
    }

    // Take field
    pub fn take_records(&mut self) -> ::protobuf::RepeatedField<Record> {
        ::std::mem::replace(&mut self.records, ::protobuf::RepeatedField::new())
    }
}

impl ::protobuf::Message for Batch {
    fn is_initialized(&self) -> bool {
        for v in &self.records {
            if !v.is_initialized() {
                return false;
            }
        };
        true
    }

    fn merge_from(&mut self, is: &mut ::protobuf::CodedInputStream<'_>) -> ::protobuf::ProtobufResult<()> {
        while !is.eof()? {
            let (field_number, wire_type) = is.read_tag_unpack()?;
            match field_number {
                1 => {
                    ::protobuf::rt::read_singular_proto3_string_into(wire_type, is, &mut self.id)?;
                },
                3 => {
                    ::protobuf::rt::read_singular_proto3_string_into(wire_type, is, &mut self.interval_start)?;
                },
                4 => {
                    ::protobuf::rt::read_singular_proto3_string_into(wire_type, is, &mut self.interval_end)?;
                },
                7 => {
                    ::protobuf::rt::read_repeated_message_into(wire_type, is, &mut self.records)?;
                },
                _ => {
                    ::protobuf::rt::read_unknown_or_skip_group(field_number, wire_type, is, self.mut_unknown_fields())?;
                },
            };
        }
        ::std::result::Result::Ok(())
    }

    // Compute sizes of nested messages
    #[allow(unused_variables)]
    fn compute_size(&self) -> u32 {
        let mut my_size = 0;
        if !self.id.is_empty() {
            my_size += ::protobuf::rt::string_size(1, &self.id);
        }
        if !self.interval_start.is_empty() {
            my_size += ::protobuf::rt::string_size(3, &self.interval_start);
        }
        if !self.interval_end.is_empty() {
            my_size += ::protobuf::rt::string_size(4, &self.interval_end);
        }
        for value in &self.records {
            let len = value.compute_size();
            my_size += 1 + ::protobuf::rt::compute_raw_varint32_size(len) + len;
        };
        my_size += ::protobuf::rt::unknown_fields_size(self.get_unknown_fields());
        self.cached_size.set(my_size);
        my_size
    }

    fn write_to_with_cached_sizes(&self, os: &mut ::protobuf::CodedOutputStream<'_>) -> ::protobuf::ProtobufResult<()> {
        if !self.id.is_empty() {
            os.write_string(1, &self.id)?;
        }
        if !self.interval_start.is_empty() {
            os.write_string(3, &self.interval_start)?;
        }
        if !self.interval_end.is_empty() {
            os.write_string(4, &self.interval_end)?;
        }
        for v in &self.records {
            os.write_tag(7, ::protobuf::wire_format::WireTypeLengthDelimited)?;
            os.write_raw_varint32(v.get_cached_size())?;
            v.write_to_with_cached_sizes(os)?;
        };
        os.write_unknown_fields(self.get_unknown_fields())?;
        ::std::result::Result::Ok(())
    }

    fn get_cached_size(&self) -> u32 {
        self.cached_size.get()
    }

    fn get_unknown_fields(&self) -> &::protobuf::UnknownFields {
        &self.unknown_fields
    }

    fn mut_unknown_fields(&mut self) -> &mut ::protobuf::UnknownFields {
        &mut self.unknown_fields
    }

    fn as_any(&self) -> &dyn (::std::any::Any) {
        self as &dyn (::std::any::Any)
    }
    fn as_any_mut(&mut self) -> &mut dyn (::std::any::Any) {
        self as &mut dyn (::std::any::Any)
    }
    fn into_any(self: ::std::boxed::Box<Self>) -> ::std::boxed::Box<dyn (::std::any::Any)> {
        self
    }

    fn descriptor(&self) -> &'static ::protobuf::reflect::MessageDescriptor {
        Self::descriptor_static()
    }

    fn new() -> Batch {
        Batch::new()
    }

    fn descriptor_static() -> &'static ::protobuf::reflect::MessageDescriptor {
        static descriptor: ::protobuf::rt::LazyV2<::protobuf::reflect::MessageDescriptor> = ::protobuf::rt::LazyV2::INIT;
        descriptor.get(|| {
            let mut fields = ::std::vec::Vec::new();
            fields.push(::protobuf::reflect::accessor::make_simple_field_accessor::<_, ::protobuf::types::ProtobufTypeString>(
                "id",
                |m: &Batch| { &m.id },
                |m: &mut Batch| { &mut m.id },
            ));
            fields.push(::protobuf::reflect::accessor::make_simple_field_accessor::<_, ::protobuf::types::ProtobufTypeString>(
                "interval_start",
                |m: &Batch| { &m.interval_start },
                |m: &mut Batch| { &mut m.interval_start },
            ));
            fields.push(::protobuf::reflect::accessor::make_simple_field_accessor::<_, ::protobuf::types::ProtobufTypeString>(
                "interval_end",
                |m: &Batch| { &m.interval_end },
                |m: &mut Batch| { &mut m.interval_end },
            ));
            fields.push(::protobuf::reflect::accessor::make_repeated_field_accessor::<_, ::protobuf::types::ProtobufTypeMessage<Record>>(
                "records",
                |m: &Batch| { &m.records },
                |m: &mut Batch| { &mut m.records },
            ));
            ::protobuf::reflect::MessageDescriptor::new_pb_name::<Batch>(
                "Batch",
                fields,
                file_descriptor_proto()
            )
        })
    }

    fn default_instance() -> &'static Batch {
        static instance: ::protobuf::rt::LazyV2<Batch> = ::protobuf::rt::LazyV2::INIT;
        instance.get(Batch::new)
    }
}

impl ::protobuf::Clear for Batch {
    fn clear(&mut self) {
        self.id.clear();
        self.interval_start.clear();
        self.interval_end.clear();
        self.records.clear();
        self.unknown_fields.clear();
    }
}

impl ::std::fmt::Debug for Batch {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        ::protobuf::text_format::fmt(self, f)
    }
}

impl ::protobuf::reflect::ProtobufValue for Batch {
    fn as_ref(&self) -> ::protobuf::reflect::ReflectValueRef {
        ::protobuf::reflect::ReflectValueRef::Message(self)
    }
}

#[derive(Clone,PartialEq,Eq,Debug,Hash)]
pub enum Resource {
    NULL = 0,
    CPU = 1,
    MEM = 2,
    DISK = 3,
}

impl ::protobuf::ProtobufEnum for Resource {
    fn value(&self) -> i32 {
        *self as i32
    }

    fn from_i32(value: i32) -> ::std::option::Option<Resource> {
        match value {
            0 => ::std::option::Option::Some(Resource::NULL),
            1 => ::std::option::Option::Some(Resource::CPU),
            2 => ::std::option::Option::Some(Resource::MEM),
            3 => ::std::option::Option::Some(Resource::DISK),
            _ => ::std::option::Option::None
        }
    }

    fn values() -> &'static [Self] {
        static values: &'static [Resource] = &[
            Resource::NULL,
            Resource::CPU,
            Resource::MEM,
            Resource::DISK,
        ];
        values
    }

    fn enum_descriptor_static() -> &'static ::protobuf::reflect::EnumDescriptor {
        static descriptor: ::protobuf::rt::LazyV2<::protobuf::reflect::EnumDescriptor> = ::protobuf::rt::LazyV2::INIT;
        descriptor.get(|| {
            ::protobuf::reflect::EnumDescriptor::new_pb_name::<Resource>("Resource", file_descriptor_proto())
        })
    }
}

impl ::std::marker::Copy for Resource {
}

impl ::std::default::Default for Resource {
    fn default() -> Self {
        Resource::NULL
    }
}

impl ::protobuf::reflect::ProtobufValue for Resource {
    fn as_ref(&self) -> ::protobuf::reflect::ReflectValueRef {
        ::protobuf::reflect::ReflectValueRef::Enum(::protobuf::ProtobufEnum::descriptor(self))
    }
}

#[derive(Clone,PartialEq,Eq,Debug,Hash)]
pub enum Units {
    NULL_UNIT = 0,
    BYTES = 1,
    MILLIS = 2,
    UNITS = 3,
}

impl ::protobuf::ProtobufEnum for Units {
    fn value(&self) -> i32 {
        *self as i32
    }

    fn from_i32(value: i32) -> ::std::option::Option<Units> {
        match value {
            0 => ::std::option::Option::Some(Units::NULL_UNIT),
            1 => ::std::option::Option::Some(Units::BYTES),
            2 => ::std::option::Option::Some(Units::MILLIS),
            3 => ::std::option::Option::Some(Units::UNITS),
            _ => ::std::option::Option::None
        }
    }

    fn values() -> &'static [Self] {
        static values: &'static [Units] = &[
            Units::NULL_UNIT,
            Units::BYTES,
            Units::MILLIS,
            Units::UNITS,
        ];
        values
    }

    fn enum_descriptor_static() -> &'static ::protobuf::reflect::EnumDescriptor {
        static descriptor: ::protobuf::rt::LazyV2<::protobuf::reflect::EnumDescriptor> = ::protobuf::rt::LazyV2::INIT;
        descriptor.get(|| {
            ::protobuf::reflect::EnumDescriptor::new_pb_name::<Units>("Units", file_descriptor_proto())
        })
    }
}

impl ::std::marker::Copy for Units {
}

impl ::std::default::Default for Units {
    fn default() -> Self {
        Units::NULL_UNIT
    }
}

impl ::protobuf::reflect::ProtobufValue for Units {
    fn as_ref(&self) -> ::protobuf::reflect::ReflectValueRef {
        ::protobuf::reflect::ReflectValueRef::Enum(::protobuf::ProtobufEnum::descriptor(self))
    }
}

static file_descriptor_proto_data: &'static [u8] = b"\
    \n!src/format/protobuf/billing.proto\"[\n\x0bMeasurement\x12%\n\x08resou\
    rce\x18\x01\x20\x01(\x0e2\t.ResourceR\x08resource\x12%\n\x0emeasured_val\
    ue\x18\x02\x20\x01(\x03R\rmeasuredValue\"\xb0\x01\n\x06Record\x12%\n\x0e\
    interval_start\x18\x01\x20\x01(\tR\rintervalStart\x12!\n\x0cinterval_end\
    \x18\x02\x20\x01(\tR\x0bintervalEnd\x12\x14\n\x05meter\x18\x03\x20\x01(\
    \tR\x05meter\x12\x14\n\x05value\x18\x04\x20\x01(\rR\x05value\x120\n\x0cm\
    easurements\x18\x07\x20\x03(\x0b2\x0c.MeasurementR\x0cmeasurements\"\x84\
    \x01\n\x05Batch\x12\x0e\n\x02id\x18\x01\x20\x01(\tR\x02id\x12%\n\x0einte\
    rval_start\x18\x03\x20\x01(\tR\rintervalStart\x12!\n\x0cinterval_end\x18\
    \x04\x20\x01(\tR\x0bintervalEnd\x12!\n\x07records\x18\x07\x20\x03(\x0b2\
    \x07.RecordR\x07records*0\n\x08Resource\x12\x08\n\x04NULL\x10\0\x12\x07\
    \n\x03CPU\x10\x01\x12\x07\n\x03MEM\x10\x02\x12\x08\n\x04DISK\x10\x03*8\n\
    \x05Units\x12\r\n\tNULL_UNIT\x10\0\x12\t\n\x05BYTES\x10\x01\x12\n\n\x06M\
    ILLIS\x10\x02\x12\t\n\x05UNITS\x10\x03b\x06proto3\
";

static file_descriptor_proto_lazy: ::protobuf::rt::LazyV2<::protobuf::descriptor::FileDescriptorProto> = ::protobuf::rt::LazyV2::INIT;

fn parse_descriptor_proto() -> ::protobuf::descriptor::FileDescriptorProto {
    ::protobuf::parse_from_bytes(file_descriptor_proto_data).unwrap()
}

pub fn file_descriptor_proto() -> &'static ::protobuf::descriptor::FileDescriptorProto {
    file_descriptor_proto_lazy.get(|| {
        parse_descriptor_proto()
    })
}
