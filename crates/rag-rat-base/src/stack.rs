//! Stack-growth guard for deep recursive tree walks (tree-sitter ASTs can nest arbitrarily).

/// When fewer than this many bytes of stack remain, [`grow_stack`] allocates a fresh segment
/// before running its closure. Comfortably larger than the deepest single (per-recursion-level)
/// frame chain so a level can never straddle the boundary and overflow before the next check.
const STACK_RED_ZONE: usize = 128 * 1024;
/// Size of each stack segment [`grow_stack`] allocates when the red zone is hit.
const STACK_SEGMENT: usize = 4 * 1024 * 1024;

/// Run `f`, first growing the stack if it is near exhaustion (#543). Wrap the body of any
/// tree-sitter descent helper that recurses to unbounded subtree depth — a callee that is thousands
/// of nested parens, a thousands-deep generic type — so parsing a hostile source file grows the
/// stack instead of overflowing the indexer's worker-thread stack (`stacker`, the mechanism rustc
/// uses). It is a no-op fast path (one stack-pointer comparison) when ample stack remains, so real
/// shallow inputs pay effectively nothing, and it does not change any output.
pub fn grow_stack<R>(f: impl FnOnce() -> R) -> R {
    stacker::maybe_grow(STACK_RED_ZONE, STACK_SEGMENT, f)
}
