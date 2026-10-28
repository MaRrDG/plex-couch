//! App logic on top of the services. Pure functions are kept testable; each module
//! names the spec section it implements in its header.

pub mod demo;
pub mod images;
pub mod movies;
pub mod notify;
pub mod plex;
pub mod power;
pub mod series;
pub mod status;
#[cfg(test)]
pub mod testutil;
pub mod torrents;
pub mod val;
