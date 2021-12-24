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

## LoopFn 

`0.3` version removes the `LoopFn` library. We can neatly resolve the problem by implementing a self-defined struct. 

In our implementation, we define it in a new file so that we can import it whenever we need it. Thus, we can mitigate this change seeminglessly in this update. 

## Type Systems and Closures 

### Methods 

The author of the last version defined some types wrapping errors and built-in `future` types in `future.rs`. 

### Mistakes 

## Future Works 
