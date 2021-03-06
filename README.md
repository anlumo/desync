# Desync

```toml
[dependencies]
desync = "0.5"
```

Desync is a library for Rust that provides a model of concurrency based around the idea of 
scheduling operations on data. This is in contrast to the traditional model where operations
are scheduled on threads with ownership of the data being passed between them.

This approach has several advantages over the traditional method:

 * It's simpler: almost the  entire set of thread methods and synchronisation primitives can 
   be replaced with the two fundamental scheduling functions, `sync()` and `desync()`. 
 * It's easier to reason about: scheduled operations are always performed in the order they're 
   queued so race conditions and similar issues due to out-of-order execution are both much rarer 
   and easier to debug.
 * It makes it easier to write highly concurrent code: desync makes moving between performing
   operations synchronously and asynchronously trivial, with no need to deal with adding code to
   start threads or communicate between them.

In addition to the two fundamental methods, desync provides methods for generating futures and
processing streams.

# Quick start

Desync provides a single type, `Desync<T>` that can be used to replace both threads and mutexes.
This type schedules operations for a contained data structure so that they are always performed
in order and optionally in the background.

Such a `Desync` object can be created like so:

```Rust
use desync::Desync;
let number = Desync::new(0);
```

It supports two main operations. `async` will schedule a new job for the object that will run
in a background thread. It's useful for deferring long-running operations and moving updates
so they can run in parallel.

```Rust
let number = Desync::new(0);
number.desync(|val| {
    // Long update here
    thread::sleep(Duration::from_millis(100));
    *val = 42;
});

// We can carry on what we're doing with the update now running in the background
```

The other operation is `sync`, which schedules a job to run synchronously on the data structure.
This is useful for retrieving values from a `Desync`.

```Rust
let new_number = number.sync(|val| *val);           // = 42
```

`Desync` objects always run operations in the order that is provided, so all operations are
serialized from the point of view of the data that they contain. When combined with the ability
to perform operations asynchronously, this provides a useful way to immediately parallelize
long-running operations.

# Working with futures

Desync has support for the `futures` library. The simplest operation is `future()`, which creates
a future that runs asynchronously on a `Desync` object but - unlike `desync()` can return a result.
It works like this:

```Rust
let future_number = number.future(|val| future::ready(*val));
assert!(executor::block_on(async { future_number.await.unwrap() }) == 42 )
```

Note that this is the equivalent of just `number.sync(|val| *val)`, so this is mainly useful for
interacting with other code that's already using futures. The `after()` function is also provided
for using the results of futures to update the contents of `Desync` data: these all preserve the
strict order-of-operations semantics, so operations scheduled after an `after` won't start until
that operation has completed.

There is also support for streams, via the `pipe_in()` and `pipe()` functions. These work on
`Arc<Desync<T>>` references and provide a way to process a stream asynchronously. These two
functions provide a powerful way to process input and also to connect `Desync` objects together
using message-passing for communication.

```Rust
let some_object = Arc::new(Desync::new(some_object));

pipe_in(Arc::clone(&number), some_stream, 
    |some_object, input| some_object.process(input));

let output_stream = pipe(Arc::clone(&number), some_stream, 
    |some_object, input| some_object.process_with_output(input));
```
