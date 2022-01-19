# Notes for Updating 

## Overview 

This note is intended to document results as well as mistakes we have got in the progress of updating a library this repository. 

Our goal is to update the library `future` from the `0.1` version to the `0.3` version. 

The major challenges in this update generally involve four aspects. 

That is, the changing and resulting conflicting naming of some variables/functions, different closure type systems, removal of `LoopFn` and `Error`-associated types. 

The major difficulty in our work is lack of references on `future` library. Here are some useful references we found. 

[official doc](https://docs.rs/futures/0.1.31/futures/future/trait.Future.html#method.and_then)

[a similar project that could be very useful for us](https://www.ncameron.org/blog/migrating-a-crate-from-futures-0-1-to-0-3/)

[Github link of the project](https://github.com/tikv/client-rust/pull/41/commits/6353dbcfe391d66714686aafab9a49e593259dfb#diff-eeffc045326f81d4c46c22f225d3df90R68)

## File Layout 

## Naming 

All problems related to naming have been resolved in our current codes. 

## LoopFn 

`0.3` version removes the `LoopFn` library. We can neatly resolve the problem by implementing a self-defined struct. 

In our implementation, we define it in a new file so that we can import it whenever we need it. Thus, we can mitigate this change seeminglessly in this update. 

## Type Systems and Closures  

The author of the last version defined some types wrapping errors and built-in `future` types in `future.rs`. To make it compatible with `0.3` type systems, we have made some adaptions. 

Most of compiling errors come from two closure functions: `and_then` and `or_else`. It is because some of closures which returned a `future` that produces a `T` rather than returning a `T` itself. 

We have tried the following three ways to resolve these error. That is, changing the input variables to meet the new type requirements, fix body of closures and getting rid of these two functions. In general, we think the frist approach is not working, while the last two can be working. 

### Changing variables 

Since this error comes from the type of variables returned by anonymous functions within the closure, we tried to change these variables. This, however, is not a correct approach. Here are some take-aways from our experience. 

Firstly, we should clarify the naming scope rules related to closure in Rust. What is written in the closure can be seen as a nested new function. Therefore, it observes to the rules of naming scope in Rust. For instance, `body` in the closure should not be messed with `body` in the input variables of the function `request_with_body`. 

Secondly, because of the first, types of input variables of closure functions are not concerning. Actually, they are, and cannot be assigned with a type manually. Instead, their types depend on the body of closures. 

That was why we moved to our second method. 

### Body of Closures 

We take `request_with_body` located in `mod.rs` as an example in the following discussion. The logistics can be applied similarly in other parts of update. 

To simplify branching and to reduce complexity of code structures, we start with `else` branch first. The error we got here is: 

```
the method `and_then` exists for opaque type `impl futures::Future`, but its trait bounds were not satisfied
```

This is caused by the necessary update of `Future` defined in `type.rs` as well as the changing trait restrictions. In 0.3 version, `and_then` requires: 

```
fn and_then<F, B>(self, f: F) -> AndThen<Self, B, F>
where
    F: FnOnce(Self::Item) -> B,
    B: IntoFuture<Error = Self::Error>,
    Self: Sized, 
```

As to future development, we highly recommend following the procedure we have adopted. That is, focusing on the simple branch, when two or more parallel branches are present. It can help to greatly reduce the amount of works wasted in the early stage. This also allows for more flexibility. 

In general, advantages and disadvantages of this method can be summarized as following. 

**Advantages: **

*Less changes needed *

*Less likely to cause bugs in other parts*

**Disadvantages: **

*resolving typing error and trait violation reporting is not trivial *

### Alternative Functions 

Considering the irrating side of the second method, we have also tried getting rid of these troubling functions completely. We hoped to find a replacement of them. That means, a different function with the same behavior, either by manually implementing it or by using other functions in the library. 

**Advantages: **

*Radically resolve typing errors and trait problems *

*we can also define new functions in a new file and make the modification neatly *

**Disadvantages: **

*more modifications are necessary *

