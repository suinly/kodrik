// These contracts are wired into the gateway and client in subsequent tasks.
#![allow(dead_code)]

pub(crate) mod config;
pub(crate) mod local;
pub(crate) mod protocol;
pub(crate) mod service;
pub(crate) mod store;
pub(crate) mod supervisor;

#[cfg(test)]
mod local_tests;

#[cfg(test)]
mod supervisor_tests;

#[cfg(test)]
mod store_tests;

#[cfg(test)]
mod service_tests;

#[cfg(test)]
mod protocol_tests;
