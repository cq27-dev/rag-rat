//! Oracle commands and detached refresh policy.
mod auto_run;
mod run;

pub(crate) use auto_run::spawn_detached_oracle_auto_run;
pub(crate) use run::oracle;
