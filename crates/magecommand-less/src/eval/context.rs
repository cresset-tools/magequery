//! The eval `Context`: innermost-first frame stack, math-on state, parens
//! stack, import bookkeeping (plan §4.1, §4.2). One `Context` per compile job
//! (`Send`-free; jobs are the parallel unit, plan §9.6). Stub until Phase 1.
