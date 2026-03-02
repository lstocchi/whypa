// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

macro_rules! generate_read_fn {
    ($fn_name: ident, $data_type: ty, $byte_type: ty, $type_size: expr, $endian_type: ident) => {
        pub fn $fn_name(input: &[$byte_type]) -> $data_type {
            assert!($type_size == std::mem::size_of::<$data_type>());
            let mut array = [0u8; $type_size];
            for (byte, read) in array.iter_mut().zip(input.iter().cloned()) {
                *byte = read as u8;
            }
            <$data_type>::$endian_type(array)
        }
    };
}

macro_rules! generate_write_fn {
    ($fn_name: ident, $data_type: ty, $byte_type: ty, $endian_type: ident) => {
        pub fn $fn_name(buf: &mut [$byte_type], n: $data_type) {
            for (byte, read) in buf
                .iter_mut()
                .zip(<$data_type>::$endian_type(n).iter().cloned())
            {
                *byte = read as $byte_type;
            }
        }
    };
}

generate_read_fn!(read_le_u16, u16, u8, 2, from_le_bytes);
generate_read_fn!(read_le_u32, u32, u8, 4, from_le_bytes);
generate_read_fn!(read_le_u64, u64, u8, 8, from_le_bytes);
generate_read_fn!(read_le_i32, i32, i8, 4, from_le_bytes);

generate_read_fn!(read_be_u16, u16, u8, 2, from_be_bytes);
generate_read_fn!(read_be_u32, u32, u8, 4, from_be_bytes);

generate_write_fn!(write_le_u16, u16, u8, to_le_bytes);
generate_write_fn!(write_le_u32, u32, u8, to_le_bytes);
generate_write_fn!(write_le_u64, u64, u8, to_le_bytes);
generate_write_fn!(write_le_i32, i32, i8, to_le_bytes);

generate_write_fn!(write_be_u16, u16, u8, to_be_bytes);
generate_write_fn!(write_be_u32, u32, u8, to_be_bytes);