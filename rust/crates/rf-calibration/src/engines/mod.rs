//! Pluggable path-loss engines evaluated on stored profiles.
//!
//! `rfcore_bullington` calls `repeaternet-rf-core` directly. The other
//! engines are written against the same curvature-adjusted profile so they
//! can later move into `rf-core` behind `rf.diffraction_model`.
