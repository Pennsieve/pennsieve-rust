// Copyright (c) 2018 Pennsieve, Inc. All Rights Reserved.

//! Library-wide type definitions.

use futures;

use crate::ps::error;

pub use crate::ps::error::{Error, ErrorKind, Result};

use futures::TryFutureExt;

// 0.1 -> 0.3: 
// change all occurrences of `Future<Item=Foo, Error=Bar>` to `Future<Output=Result<Foo, Bar>>` 

/// A `futures::future::Future` type parameterized by `ps::error::Error`
trait NewTrait {}

#[allow(dead_code)]
pub type Future<T> = <Box<(dyn NewTrait + 'static)> as Box>::Pin;

impl<T> NewTrait for Future<T>
where
	Future<T>: futures::Future<Output = Result<T>> + TryFutureExt + Send 
{}

/// A `futures::stream::Stream` type parameterized by `ps::error::Error`
#[allow(dead_code)]
pub type Stream<T> = Box<dyn futures::stream::Stream<Item = T> + Send>;
