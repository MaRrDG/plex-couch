//! App logic on top of the services. Pure functions are kept testable; the Python module
//! each one ports is named in its header.

pub mod demo;
pub mod images;
pub mod movies;
pub mod notify;
pub mod plex;
pub mod power;
pub mod status;
#[cfg(test)]
pub mod testutil;
pub mod torrents;
pub mod val;
