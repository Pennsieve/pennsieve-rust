// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

//! This module contains utility types and functions for making the transition
//! from futures 0.1 to 1.0 easier.

use futures::prelude::{Future, Sink, Stream};
use futures::stream::Fuse;
use futures::task::{Context, Poll};
use futures::ready;
use std::pin::Pin;

/// The status of a `loop_fn` loop.
#[derive(Debug)]
pub(crate) enum Loop<T, S> {
	/// Indicates that the loop has completed with output `T`.
	Break(T),

	/// Indicates that the loop function should be called again with input
	/// state `S`.
	Continue(S),
}

/// A future implementing a tail-recursive loop.
///
/// Created by the `loop_fn` function.
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub(crate) struct LoopFn<A, F> {
	future: A,
	func: F,
}

/// Creates a new future implementing a tail-recursive loop.
pub(crate) fn loop_fn<S, T, A, F, E>(initial_state: S, mut func: F) -> LoopFn<A, F>
where
	F: FnMut(S) -> A,
	A: Future<Output = Result<Loop<T, S>, E>>,
{
	LoopFn {
		future: func(initial_state),
		func,
	}
}

impl<S, T, A, F, E> Future for LoopFn<A, F>
where
	F: FnMut(S) -> A,
	A: Future<Output = Result<Loop<T, S>, E>>,
{
	type Output = Result<T, E>;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<T, E>> {
		loop {
			unsafe {
				let this = Pin::get_unchecked_mut(self);
				match ready!(Pin::new_unchecked(&mut this.future).poll(cx)) {
					Loop::Break(x) => return Poll::Ready(Ok(x)),
					Loop::Continue(s) => this.future = (this.func)(s),
				}
				self = Pin::new_unchecked(this);
			}
		}
	}
}

