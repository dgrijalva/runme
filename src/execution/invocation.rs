//! `Invocation` — how a task body is dispatched at `spawn_body` time.
//!
//! Two front-ends to one runtime:
//! - `Strings` is the dynamic / string-keyed path (today's `ctx.run`,
//!   MCP, CLI re-entry). The engine calls `task_def.func` with
//!   stringified args.
//! - `Factory` is the typed path emitted by `#[rnme::task]`'s shim.
//!   Typed args are captured by value in the closure; the closure
//!   resolves the body symbol and produces the boxed future. The engine
//!   awaits the returned future the same way it awaits `task.func.call`.
//!
//! Both variants converge on `EngineInternals::spawn_child` and produce
//! identical `TaskHandle`s. See `docs/invoking_tasks.md` for the design.

use std::future::Future;
use std::pin::Pin;

use crate::error::TaskResult;
use crate::task::TaskContext;

/// A factory that, given the freshly-built `TaskContext` for the child,
/// produces the boxed future that *is* the task body.
///
/// The HRTB on `&'a TaskContext` mirrors `TaskFn` (`src/task.rs`): the
/// engine constructs the `TaskContext` inside `spawn_body`, then hands
/// a borrow to the factory. The returned future's lifetime is tied to
/// that borrow.
pub type FutureFactory = Box<
    dyn for<'a> FnOnce(&'a TaskContext) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>>
        + Send,
>;

/// How a task body is invoked.
pub enum Invocation {
    /// Stringified args, dispatched through `task_def.func`.
    Strings(Vec<String>),
    /// Typed-args closure, dispatched directly to the renamed body symbol.
    Factory(FutureFactory),
}
