//! All of rpm-health's logic lives in this library. It never prints anything,
//! so the CLI today and a GTK front-end later can both sit on top of it.

pub mod error;
pub mod model;
pub mod rpm;
