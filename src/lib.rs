#![feature(optin_builtin_traits)]
#![feature(box_patterns)]
#![feature(box_syntax)]
#![feature(slice_patterns)]
#![feature(from_ref)]
#![feature(range_contains)]

#[macro_use]
mod handle_map;
mod index_map;
mod bit_vec;
pub mod symbol;

pub mod front;
pub mod back;
pub mod vm;
