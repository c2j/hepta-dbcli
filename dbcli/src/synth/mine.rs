//! Conditional business-rule mining for `rules-draft` (issue #69).
//!
//! Scans categorical column pairs for observable `A=a => B=b` dependencies
//! and reports them as candidates. Contract: see issue #69. Candidates are
//! never enabled automatically.
