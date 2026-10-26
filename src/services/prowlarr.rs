//! Prowlarr (spec 5.4): `GET {prowlarr.url}/ping`, no key, 2.5 s, redirects followed.

use crate::services::{Client, UpResult, TIMEOUT};
use reqwest::Method;

pub struct Prowlarr {
    pub c: Client,
}

impl Prowlarr {
    pub fn new(c: Client) -> Prowlarr {
        Prowlarr { c }
    }

    /// Up when this is Ok (HTTP >= 400 counts as down).
    pub async fn ping(&self) -> UpResult<()> {
        let url = self.c.url("prowlarr", "/ping");
        self.c.http.request(Method::GET, &url, &[], None, TIMEOUT).await.map(|_| ())
    }
}
