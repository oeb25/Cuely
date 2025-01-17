// Cuely is an open source web search engine.
// Copyright (C) 2022 Cuely ApS
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use crate::{
    kv::{rocksdb_store::RocksDbStore, Kv},
    prehashed::{hash, Prehashed},
    webpage::Url,
};
use std::{collections::HashSet, path::Path};

pub struct SubdomainCounter {
    inner: Box<dyn Kv<Prehashed, HashSet<String>>>,
}

impl SubdomainCounter {
    pub fn open<P: AsRef<Path>>(path: P) -> Self {
        Self {
            inner: RocksDbStore::open(path),
        }
    }

    pub fn increment(&mut self, url: Url) {
        if let Some(subdomain) = url.subdomain() {
            let domain = hash(url.domain());
            let subdomain = subdomain.to_string();

            let mut set = self.inner.get(&domain).unwrap_or_default();
            set.insert(subdomain);
            self.inner.insert(domain, set);
        }
    }

    pub fn commit(&self) {
        self.inner.flush();
    }

    pub fn merge(&mut self, other: Self) {
        for (key, val) in other.inner.iter() {
            let mut current = self.inner.get(&key).unwrap_or_default();
            current.extend(val.into_iter());
            self.inner.insert(key, current);
        }
    }
}
