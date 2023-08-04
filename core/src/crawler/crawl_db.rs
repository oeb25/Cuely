// Stract is an open source web search engine.
// Copyright (C) 2023 Stract ApS
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

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BinaryHeap, HashMap, VecDeque},
    hash::Hash,
    num::NonZeroUsize,
    path::Path,
};

use itertools::Itertools;
use lru::LruCache;
use rand::Rng;
use rocksdb::BlockBasedOptions;
use url::Url;

use super::{Domain, Job, JobResponse, Result, UrlResponse};

#[derive(Clone, PartialEq, Eq)]
pub enum UrlStatus {
    Pending,
    Crawling,
    Failed { status_code: Option<u16> },
    Done,
}

#[derive(Clone, PartialEq, Eq)]
pub enum DomainStatus {
    Pending,
    CrawlInProgress,
}

struct IdTable<T> {
    t2id: rocksdb::DB,
    id2t: rocksdb::DB,

    next_id: u64,

    t2id_cache: LruCache<T, u64>,
    id2t_cache: LruCache<u64, T>,

    _marker: std::marker::PhantomData<T>,
}

impl<T> IdTable<T>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Hash + Eq + Clone,
{
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut options = rocksdb::Options::default();
        options.create_if_missing(true);
        options.increase_parallelism(8);
        options.set_write_buffer_size(256 * 1024 * 1024); // 256 MB memtable
        options.set_max_write_buffer_number(8);

        let mut block_options = BlockBasedOptions::default();
        block_options.set_ribbon_filter(10.0);

        options.set_block_based_table_factory(&block_options);

        // create dir if not exists
        std::fs::create_dir_all(path.as_ref())?;

        let _ = rocksdb::DB::destroy(&options, path.as_ref().join("t2id"));
        let _ = rocksdb::DB::destroy(&options, path.as_ref().join("id2t"));

        let t2id = rocksdb::DB::open(&options, path.as_ref().join("t2id"))?;
        let id2t = rocksdb::DB::open(&options, path.as_ref().join("id2t"))?;

        Ok(Self {
            t2id,
            id2t,
            next_id: 0,

            t2id_cache: LruCache::new(NonZeroUsize::new(500_000).unwrap()),
            id2t_cache: LruCache::new(NonZeroUsize::new(500_000).unwrap()),

            _marker: std::marker::PhantomData,
        })
    }

    pub fn bulk_ids<'a>(&'a mut self, items: impl Iterator<Item = &'a T>) -> Result<Vec<u64>> {
        let mut ids = Vec::new();

        let mut batch_id2t = rocksdb::WriteBatch::default();
        let mut batch_t2id = rocksdb::WriteBatch::default();

        let mut batch_assignments = HashMap::new();

        for item in items {
            // check cache
            if let Some(id) = self.t2id_cache.get(item) {
                ids.push(*id);
                continue;
            }

            // check if item exists
            let item_bytes = bincode::serialize(item)?;
            let id = self.t2id.get(&item_bytes)?;
            if let Some(id) = id {
                let id = bincode::deserialize(&id)?;

                // update cache
                self.t2id_cache.put(item.clone(), id);

                ids.push(id);
                continue;
            }

            // insert item
            let assigned_id = batch_assignments.get(item).copied();

            let id = assigned_id.unwrap_or_else(|| {
                let id = self.next_id;
                self.next_id += 1;
                id
            });

            let id_bytes = bincode::serialize(&id)?;
            batch_t2id.put(&item_bytes, &id_bytes);
            batch_id2t.put(&id_bytes, &item_bytes);

            if assigned_id.is_none() {
                batch_assignments.insert(item.clone(), id);
            }

            // update cache
            self.t2id_cache.put(item.clone(), id);

            ids.push(id);
        }

        let mut write_options = rocksdb::WriteOptions::default();
        write_options.set_sync(false);
        write_options.disable_wal(true);

        self.id2t.write_opt(batch_id2t, &write_options)?;
        self.t2id.write_opt(batch_t2id, &write_options)?;

        Ok(ids)
    }

    pub fn id(&mut self, item: T) -> Result<u64> {
        // check cache
        if let Some(id) = self.t2id_cache.get(&item) {
            return Ok(*id);
        }

        // check if item exists
        let item_bytes = bincode::serialize(&item)?;
        let id = self.t2id.get(&item_bytes)?;
        if let Some(id) = id {
            let id = bincode::deserialize(&id)?;

            // update cache
            self.t2id_cache.put(item.clone(), id);
            self.id2t_cache.put(id, item);

            return Ok(id);
        }

        // insert item
        let id = self.next_id;
        self.next_id += 1;
        let id_bytes = bincode::serialize(&id)?;

        let mut write_options = rocksdb::WriteOptions::default();
        write_options.set_sync(false);
        write_options.disable_wal(true);

        self.t2id.put_opt(&item_bytes, &id_bytes, &write_options)?;
        self.id2t.put_opt(&id_bytes, &item_bytes, &write_options)?;

        // update cache
        self.t2id_cache.put(item.clone(), id);
        self.id2t_cache.put(id, item);

        Ok(id)
    }

    pub fn value(&mut self, id: u64) -> Result<Option<T>> {
        // check cache
        if let Some(value) = self.id2t_cache.get(&id) {
            return Ok(Some(value.clone()));
        }

        let id_bytes = bincode::serialize(&id)?;
        let value_bytes = self.id2t.get(id_bytes)?;
        if let Some(value_bytes) = value_bytes {
            let value: T = bincode::deserialize(&value_bytes)?;

            // update cache
            self.t2id_cache.put(value.clone(), id);
            self.id2t_cache.put(id, value.clone());

            return Ok(Some(value));
        }

        Ok(None)
    }
}

struct SampledItem<'a, T> {
    item: &'a T,
    priority: f64,
}

impl<'a, T> PartialEq for SampledItem<'a, T> {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority
    }
}

impl<'a, T> Eq for SampledItem<'a, T> {}

impl<'a, T> PartialOrd for SampledItem<'a, T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.priority.partial_cmp(&other.priority)
    }
}

impl<'a, T> Ord for SampledItem<'a, T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority
            .partial_cmp(&other.priority)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

fn weighted_sample<'a, T: 'a>(
    items: impl Iterator<Item = (&'a T, f64)>,
    num_items: usize,
) -> Vec<&'a T> {
    let mut sampled_items: BinaryHeap<SampledItem<T>> = BinaryHeap::with_capacity(num_items);

    let mut rng = rand::thread_rng();

    for (item, weight) in items {
        // see https://www.kaggle.com/code/kotamori/random-sample-with-weights-on-sql/notebook for details on math
        let priority = -(rng.gen::<f64>().abs() + f64::EPSILON).ln() / (weight + 1.0);

        if sampled_items.len() < num_items {
            sampled_items.push(SampledItem { item, priority });
        } else if let Some(mut max) = sampled_items.peek_mut() {
            if priority < max.priority {
                max.item = item;
                max.priority = priority;
            }
        }
    }

    sampled_items.into_iter().map(|s| s.item).collect()
}

struct UrlState {
    weight: f64,
    status: UrlStatus,
}
struct DomainState {
    weight: f64,
    status: DomainStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DomainId(u64);

impl From<u64> for DomainId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct UrlId(u64);

impl From<u64> for UrlId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

pub struct RedirectDb {
    inner: rocksdb::DB,
}

impl RedirectDb {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut options = rocksdb::Options::default();
        options.create_if_missing(true);

        let mut block_options = BlockBasedOptions::default();
        block_options.set_ribbon_filter(10.0);
        options.set_block_based_table_factory(&block_options);

        let inner = rocksdb::DB::open(&options, path.as_ref())?;

        Ok(Self { inner })
    }

    pub fn put(&self, from: &Url, to: &Url) -> Result<()> {
        let url_bytes = bincode::serialize(from)?;
        let redirect_bytes = bincode::serialize(to)?;

        let mut write_options = rocksdb::WriteOptions::default();
        write_options.set_sync(false);
        write_options.disable_wal(true);
        self.inner
            .put_opt(url_bytes, redirect_bytes, &write_options)?;

        Ok(())
    }

    pub fn get(&self, from: &Url) -> Result<Option<Url>> {
        let url_bytes = bincode::serialize(from)?;
        let redirect_bytes = self.inner.get(url_bytes)?;

        if let Some(redirect_bytes) = redirect_bytes {
            let redirect: Url = bincode::deserialize(&redirect_bytes)?;
            return Ok(Some(redirect));
        }

        Ok(None)
    }
}

struct UrlToInsert {
    url: Url,
    different_domain: bool,
}

pub struct CrawlDb {
    url_ids: IdTable<Url>,
    domain_ids: IdTable<Domain>,

    redirects: RedirectDb,

    domain_state: BTreeMap<DomainId, DomainState>,

    urls: BTreeMap<DomainId, BTreeMap<UrlId, UrlState>>,
}

impl CrawlDb {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let url_ids = IdTable::open(path.as_ref().join("urls"))?;
        let domain_ids = IdTable::open(path.as_ref().join("domains"))?;
        let redirects = RedirectDb::open(path.as_ref().join("redirects"))?;

        Ok(Self {
            url_ids,
            domain_ids,
            redirects,
            domain_state: BTreeMap::new(),
            urls: BTreeMap::new(),
        })
    }

    pub fn insert_seed_urls(&mut self, urls: &[Url]) -> Result<()> {
        for url in urls {
            let domain_id = self.domain_ids.id(url.into())?.into();
            let url_id = self.url_ids.id(url.clone())?.into();

            self.domain_state
                .entry(domain_id)
                .or_insert_with(|| DomainState {
                    weight: 0.0,
                    status: DomainStatus::Pending,
                });

            self.urls.entry(domain_id).or_default().insert(
                url_id,
                UrlState {
                    weight: 0.0,
                    status: UrlStatus::Pending,
                },
            );
        }

        Ok(())
    }

    pub fn insert_urls(&mut self, responses: &[JobResponse]) -> Result<()> {
        let mut domains: HashMap<Domain, Vec<UrlToInsert>> = HashMap::new();

        for res in responses {
            for url in &res.discovered_urls {
                let domain = Domain::from(url);
                let different_domain = res.domain != domain;

                domains.entry(domain).or_default().push(UrlToInsert {
                    url: url.clone(),
                    different_domain,
                });
            }
        }

        let domain_ids: Vec<DomainId> = self
            .domain_ids
            .bulk_ids(domains.keys())?
            .into_iter()
            .map(DomainId::from)
            .collect();

        self.url_ids
            .bulk_ids(domains.values().flatten().map(|u| &u.url))?;

        for (domain_id, urls) in domain_ids.into_iter().zip_eq(domains.values()) {
            let domain_state = self
                .domain_state
                .entry(domain_id)
                .or_insert_with(|| DomainState {
                    weight: 0.0,
                    status: DomainStatus::Pending,
                });

            let url_states = self.urls.entry(domain_id).or_default();

            for url in urls {
                let url_id: UrlId = self.url_ids.id(url.url.clone())?.into();

                let url_state = url_states.entry(url_id).or_insert_with(|| UrlState {
                    weight: 0.0,
                    status: UrlStatus::Pending,
                });

                if url.different_domain {
                    url_state.weight += 1.0;
                }

                if url_state.weight > domain_state.weight {
                    domain_state.weight = url_state.weight;
                }
            }
        }

        Ok(())
    }

    pub fn update_url_status(&mut self, job_responses: &[JobResponse]) -> Result<()> {
        let mut url_responses: HashMap<Domain, Vec<UrlResponse>> = HashMap::new();

        for res in job_responses {
            for url_response in &res.url_responses {
                match url_response {
                    UrlResponse::Success { url } => {
                        let domain = Domain::from(url);
                        url_responses
                            .entry(domain)
                            .or_default()
                            .push(url_response.clone());
                    }
                    UrlResponse::Failed {
                        url,
                        status_code: _,
                    } => {
                        let domain = Domain::from(url);
                        url_responses
                            .entry(domain)
                            .or_default()
                            .push(url_response.clone());
                    }
                    UrlResponse::Redirected { url, new_url: _ } => {
                        let domain = Domain::from(url);
                        url_responses
                            .entry(domain)
                            .or_default()
                            .push(url_response.clone());
                    }
                }
            }
        }

        // bulk register urls
        self.url_ids
            .bulk_ids(url_responses.values().flatten().flat_map(|res| match res {
                UrlResponse::Success { url } => vec![url].into_iter(),
                UrlResponse::Failed {
                    url,
                    status_code: _,
                } => vec![url].into_iter(),
                UrlResponse::Redirected { url, new_url } => vec![url, new_url].into_iter(),
            }))?;

        // bulk register domains
        self.domain_ids.bulk_ids(url_responses.keys())?;

        for (domain, responses) in url_responses {
            let domain_id: DomainId = self.domain_ids.id(domain.clone())?.into();

            self.domain_state
                .entry(domain_id)
                .or_insert_with(|| DomainState {
                    weight: 0.0,
                    status: DomainStatus::Pending,
                });

            let url_states = self.urls.entry(domain_id).or_default();

            for response in responses {
                match response {
                    UrlResponse::Success { url } => {
                        let url_id: UrlId = self.url_ids.id(url.clone())?.into();

                        let url_state = url_states.entry(url_id).or_insert_with(|| UrlState {
                            weight: 0.0,
                            status: UrlStatus::Pending,
                        });

                        url_state.status = UrlStatus::Done;
                    }
                    UrlResponse::Failed { url, status_code } => {
                        let url_id: UrlId = self.url_ids.id(url.clone())?.into();

                        let url_state = url_states.entry(url_id).or_insert_with(|| UrlState {
                            weight: 0.0,
                            status: UrlStatus::Pending,
                        });

                        url_state.status = UrlStatus::Failed { status_code };
                    }
                    UrlResponse::Redirected { url, new_url } => {
                        self.redirects.put(&url, &new_url)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn set_domain_status(&mut self, domain: &Domain, status: DomainStatus) -> Result<()> {
        let domain_id: DomainId = self.domain_ids.id(domain.clone())?.into();

        let domain_state = self
            .domain_state
            .entry(domain_id)
            .or_insert_with(|| DomainState {
                weight: 0.0,
                status: status.clone(),
            });

        domain_state.status = status;

        Ok(())
    }

    pub fn sample_domains(&mut self, num_jobs: usize) -> Result<Vec<DomainId>> {
        let sampled = weighted_sample(
            self.domain_state.iter().filter_map(|(id, state)| {
                if state.status == DomainStatus::Pending {
                    Some((id, state.weight))
                } else {
                    None
                }
            }),
            num_jobs,
        )
        .into_iter()
        .copied()
        .collect();

        for id in &sampled {
            let state = self.domain_state.get_mut(id).unwrap();
            state.status = DomainStatus::CrawlInProgress;
        }

        Ok(sampled)
    }

    pub fn prepare_jobs(&mut self, domains: &[DomainId], urls_per_job: usize) -> Result<Vec<Job>> {
        let mut jobs = Vec::with_capacity(domains.len());
        for domain_id in domains {
            let urls = self.urls.entry(*domain_id).or_default();

            let sampled: Vec<_> = weighted_sample(
                urls.iter_mut().filter_map(|(id, state)| {
                    if state.status == UrlStatus::Pending {
                        Some((id, state.weight))
                    } else {
                        None
                    }
                }),
                urls_per_job,
            )
            .into_iter()
            .copied()
            .collect();

            for id in &sampled {
                let state = urls.get_mut(id).unwrap();
                state.status = UrlStatus::Crawling;
            }

            let domain_state = self.domain_state.get_mut(domain_id).unwrap();

            domain_state.weight = urls
                .iter()
                .filter_map(|(_, state)| {
                    if state.status == UrlStatus::Pending {
                        Some(state.weight)
                    } else {
                        None
                    }
                })
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
                .unwrap_or(0.0);

            let mut job = Job {
                domain: self.domain_ids.value(domain_id.0)?.unwrap(),
                fetch_sitemap: false, // todo: fetch for new sites
                urls: VecDeque::with_capacity(urls_per_job),
            };

            for url_id in sampled {
                let url = self.url_ids.value(url_id.0)?.unwrap();
                job.urls.push_back(url);
            }

            jobs.push(job);
        }

        Ok(jobs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling() {
        let items: Vec<(usize, f64)> = vec![(0, 1.0), (1, 2.0), (2, 3.0), (3, 4.0)];
        let sampled = weighted_sample(items.iter().map(|(i, w)| (i, *w)), 10);
        assert_eq!(sampled.len(), items.len());

        let items: Vec<(usize, f64)> = vec![(0, 1.0), (1, 2.0), (2, 3.0), (3, 4.0)];
        let sampled = weighted_sample(items.iter().map(|(i, w)| (i, *w)), 1);
        assert_eq!(sampled.len(), 1);

        let items: Vec<(usize, f64)> = vec![(0, 1.0), (1, 2.0), (2, 3.0), (3, 4.0)];
        let sampled = weighted_sample(items.iter().map(|(i, w)| (i, *w)), 0);
        assert_eq!(sampled.len(), 0);

        let items: Vec<(usize, f64)> = vec![(0, 1000000000.0), (1, 2.0)];
        let sampled = weighted_sample(items.iter().map(|(i, w)| (i, *w)), 1);
        assert_eq!(sampled.len(), 1);
        assert_eq!(*sampled[0], 0);
    }
}
