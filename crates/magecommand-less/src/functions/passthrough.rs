//! Unknown-function passthrough (plan §2.7, §4.8): any call whose name is not in
//! the registry (`translateX`, `clamp`, `var`, `env`, `rotate`, `cubic-bezier`,
//! incompatible-unit `min`/`max`, …) evaluates its args and re-emits
//! `name(evaluated-args)` verbatim. Mandatory default `Call.eval` fallthrough.
//! Stub until Phase 3.
